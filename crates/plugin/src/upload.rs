//! `http.upload`：把「用户自己挑的那一个文件」流式发到一个 URL。
//!
//! 插件只能说「面板里哪一栏」，不能说路径。路径只可能来自本模块的授权表，
//! 写这张表的唯一入口是 [`grant`]——由宿主自己弹出的文件对话框、在用户按下
//! 「打开」之后调用。表里没有的那一栏直接拒绝，而且拒绝发生在建立连接之前，
//! 一个字节都不会离开本机。
//!
//! 一次挑选只授权一次发送：发送前授权就被取走（见 `claim`），所以失败之后要
//! 让用户重新挑一次，而不是由插件自己重试。
//!
//! 这里没有、也不许出现任何具体站点或插件的知识：URL、表单字段、文件名全都
//! 由调用方的 `need` 参数给出，本模块只负责「按规矩发出去」。

use anyhow::{bail, Context, Result};
use futures_util::stream::{self, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{self, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// 插件没说要多大时，单个文件的默认上限。
const DEFAULT_FILE_CAP: u64 = 64 * 1024 * 1024;
/// 单个文件的硬上限：再大就不是「顺手传个文件」了，宿主不给。
const MAX_FILE_CAP: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// 插件没说要多大时，回包正文的默认上限。
const DEFAULT_REPLY_CAP: usize = 64 * 1024;
const MAX_REPLY_CAP: usize = 1024 * 1024;
/// 表单最多几个字段：正常上传是 3-5 个，32 已经给足了余量。
const MAX_FIELDS: usize = 32;
/// 用户挑完文件后多久之内必须发出去。过期就当作没挑过。
const GRANT_TTL: Duration = Duration::from_secs(30 * 60);
/// 读写粒度，也是摘要和发送的粒度。
const CHUNK: usize = 128 * 1024;

/// 宿主替用户记下的一次挑选。
struct Grant {
    path: PathBuf,
    len: u64,
    mtime: Option<SystemTime>,
    picked: Instant,
}

/// 正在跑的一次上传，用来接收取消信号。
struct Live {
    plugin: String,
    stop: tokio::sync::watch::Sender<bool>,
}

struct State {
    grants: Mutex<HashMap<(String, String), Grant>>,
    live: Mutex<HashMap<u64, Live>>,
    busy: AtomicBool,
    seq: AtomicU64,
}

fn state() -> &'static State {
    static S: OnceLock<State> = OnceLock::new();
    S.get_or_init(|| State {
        grants: Mutex::new(HashMap::new()),
        live: Mutex::new(HashMap::new()),
        busy: AtomicBool::new(false),
        seq: AtomicU64::new(0),
    })
}

/// 记下一次用户挑选。`plugin` 来自宿主注入的 `_plugin`，不是插件自己说的。
///
/// 同一 `(plugin, form_key)` 只保留最新一次；同一栏再挑一次就是替换。
/// `path` 必须是已经存在的普通文件，否则这一步就失败——用户对话框里选出来的
/// 目录、已经不存在的文件都不该进入授权表。
pub fn grant(plugin: &str, form_key: &str, path: &Path) -> Result<()> {
    let plugin = plugin.trim();
    if plugin.is_empty() {
        bail!("{}", rcgui_i18n::t("UPLOAD_BAD_PARAM: 缺少插件 id"));
    }
    if !is_key(form_key) {
        bail!(
            "{}",
            rcgui_i18n::f("UPLOAD_BAD_PARAM: 表单字段名不合法：{}", &[&form_key])
        );
    }
    let meta = std::fs::metadata(path)
        .with_context(|| rcgui_i18n::f("读取 {} 失败", &[&path.display()]))?;
    if !meta.is_file() {
        bail!(
            "{}",
            rcgui_i18n::f("只能上传磁盘上的普通文件：{}", &[&path.display()])
        );
    }
    state().grants.lock().unwrap().insert(
        (plugin.to_string(), form_key.to_string()),
        Grant {
            path: path.to_path_buf(),
            len: meta.len(),
            mtime: meta.modified().ok(),
            picked: Instant::now(),
        },
    );
    Ok(())
}

/// 撤掉一栏的授权（用户重选了别的、或把面板关掉了）。
pub fn revoke(plugin: &str, form_key: &str) {
    state()
        .grants
        .lock()
        .unwrap()
        .remove(&(plugin.trim().to_string(), form_key.to_string()));
}

/// 撤掉一个插件的全部授权（卸载、停用、切走面板时用）。
pub fn revoke_plugin(plugin: &str) {
    let plugin = plugin.trim();
    state()
        .grants
        .lock()
        .unwrap()
        .retain(|(p, _), _| p != plugin);
}

/// 取消所有正在进行的上传（退出程序时用）。
pub fn cancel_all() {
    for live in state().live.lock().unwrap().values() {
        let _ = live.stop.send(true);
    }
}

/// 取消某个插件的上传（面板被关掉、插件被卸载时用）。
pub fn cancel_plugin(plugin: &str) {
    let plugin = plugin.trim();
    for live in state().live.lock().unwrap().values() {
        if live.plugin == plugin {
            let _ = live.stop.send(true);
        }
    }
}

/// `http.upload` 本体。`plugin` 是宿主注入的插件 id（`_plugin`）。
///
/// 参数见 `docs/PLUGIN.md` 的 H8 小节；返回 `{status, body, size, bytes_sent, truncated}`。
/// 任何一步失败都返回 `Err`，由调用方包成 `{"_error": ...}` 还给插件；
/// 错误文本里只有脱敏后的地址，没有请求头，也就没有凭据。
pub async fn upload(plugin: &str, params: &Value) -> Result<Value> {
    let plugin = plugin.trim();
    if plugin.is_empty() {
        bail!("{}", rcgui_i18n::t("UPLOAD_BAD_PARAM: 缺少插件 id"));
    }
    let plan = Plan::parse(params)?;
    // 同一时刻只跑一个上传：不必为并发上传做配额，也不会让多个大文件抢带宽。
    let _busy = Busy::acquire()?;
    let claimed = claim(plugin, &plan.form_key)?;
    let (len, digest, file) = hash_file(&claimed.path, plan.file_cap, claimed.len).await?;

    let boundary = format!(
        "----rcgui{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    );
    let file_name = plan
        .file_name
        .clone()
        .unwrap_or_else(|| claimed.name.clone());

    // 表单字段按插件给的顺序排在文件段之前：站点要求这样，而且顺序可预测，
    // 插件复算不出摘要时也能靠它对齐。
    let mut pre: Vec<u8> = Vec::with_capacity(1024);
    for part in &plan.fields {
        pre.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n{}\r\n",
                boundary, part.name, part.value
            )
            .as_bytes(),
        );
    }
    pre.extend_from_slice(
        format!(
            "--{}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
            boundary, plan.part_name, file_name, plan.mime
        )
        .as_bytes(),
    );
    let post = format!("\r\n--{}--\r\n", boundary).into_bytes();
    let body_len = pre.len() as u64 + len + post.len() as u64;

    let head = stream::once(std::future::ready(Ok::<Vec<u8>, io::Error>(pre)));
    let tail = stream::once(std::future::ready(Ok::<Vec<u8>, io::Error>(post)));
    let file_stream = stream::unfold(Some(file), |slot| async move {
        let mut file = slot?;
        let mut buf = vec![0u8; CHUNK];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<Vec<u8>, io::Error>(buf), Some(file)))
            }
            Err(e) => Some((Err(e), None)),
        }
    });
    let body = reqwest::Body::wrap_stream(head.chain(file_stream).chain(tail));

    let client = reqwest::Client::builder()
        // 上传不跟跳转：目标地址是用户看过的那个，跳走就等于发到别处去了。
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(plan.timeout)
        .user_agent(format!("RCGUI/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("http 客户端创建失败")?;

    let mut req = client
        .request(plan.method.clone(), plan.url.as_str())
        // 这三个头由宿主决定：插件设了会在校验阶段被挡回去。
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={}", boundary),
        )
        .header(reqwest::header::CONTENT_LENGTH, body_len.to_string())
        .body(body);
    for (name, value) in &plan.headers {
        req = req.header(name.as_str(), value.as_str());
    }
    if let Some(seed) = &plan.idem_seed {
        req = req.header("idempotency-key", format!("{}-{}", seed, digest));
    }

    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let live_id = register(plugin, tx);
    let send = req.send();
    tokio::pin!(send);
    let outcome = tokio::select! {
        r = &mut send => Some(r),
        _ = rx.changed() => None,
    };
    unregister(live_id);
    let Some(outcome) = outcome else {
        bail!("{}", rcgui_i18n::t("CANCELLED: 上传已取消"));
    };

    let mut resp = outcome.with_context(|| rcgui_i18n::f("上传失败：{}", &[&redact(&plan.url)]))?;
    let status = resp.status();
    if status.is_redirection() {
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(服务器没给 Location)");
        bail!(
            "{}",
            rcgui_i18n::f(
                "UPLOAD_REDIRECT: 服务器要求跳转到 {}，宿主不跟随",
                &[&redact(location)]
            )
        );
    }

    let mut reply: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => bail!(
                "{}",
                rcgui_i18n::f("读取 {} 的响应失败：{}", &[&redact(&plan.url), &e])
            ),
        };
        if reply.len() + chunk.len() > plan.reply_cap {
            let room = plan.reply_cap.saturating_sub(reply.len());
            reply.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        reply.extend_from_slice(&chunk);
    }

    Ok(json!({
        "status": status.as_u16(),
        "body": String::from_utf8_lossy(&reply),
        "size": len,
        "bytes_sent": body_len,
        "truncated": truncated,
    }))
}

/// 一次上传期间占住「只允许一个」的名额，作用域结束自动放开。
struct Busy;

impl Busy {
    fn acquire() -> Result<Busy> {
        if state().busy.swap(true, Ordering::SeqCst) {
            bail!("{}", rcgui_i18n::t("UPLOAD_BUSY: 已经有一个上传在进行"));
        }
        Ok(Busy)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        state().busy.store(false, Ordering::SeqCst);
    }
}

fn register(plugin: &str, stop: tokio::sync::watch::Sender<bool>) -> u64 {
    let id = state().seq.fetch_add(1, Ordering::Relaxed) + 1;
    state().live.lock().unwrap().insert(
        id,
        Live {
            plugin: plugin.to_string(),
            stop,
        },
    );
    id
}

fn unregister(id: u64) {
    state().live.lock().unwrap().remove(&id);
}

/// 取走一次授权。取走就是取走：这条路径之后再想发，得让用户重新挑。
struct Claimed {
    path: PathBuf,
    len: u64,
    name: String,
}

fn claim(plugin: &str, form_key: &str) -> Result<Claimed> {
    let key = (plugin.to_string(), form_key.to_string());
    let Some(grant) = state().grants.lock().unwrap().remove(&key) else {
        bail!(
            "{}",
            rcgui_i18n::f(
                "SOURCE_NOT_GRANTED: {} 这一栏没有用户挑过的文件，请在面板里重新选择",
                &[&form_key]
            )
        );
    };
    if grant.picked.elapsed() > GRANT_TTL {
        bail!(
            "{}",
            rcgui_i18n::f(
                "SOURCE_NOT_GRANTED: {} 的挑选已过期，请重新选择",
                &[&form_key]
            )
        );
    }
    // 挑完到发送之间文件可能被换掉：大小或修改时间变了就当作没挑过。
    let meta = std::fs::metadata(&grant.path)
        .with_context(|| rcgui_i18n::f("读取 {} 失败", &[&grant.path.display()]))?;
    if !meta.is_file() || meta.len() != grant.len || meta.modified().ok() != grant.mtime {
        bail!(
            "{}",
            rcgui_i18n::f(
                "文件在挑选之后被改过，请重新选择：{}",
                &[&grant.path.display()]
            )
        );
    }
    let name = grant
        .path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Claimed {
        path: grant.path,
        len: grant.len,
        name,
    })
}

/// 边读边算 sha256，同时确认字节数没超过上限、也没变过。返回时文件指针回到开头。
async fn hash_file(
    path: &Path,
    cap: u64,
    expect_len: u64,
) -> Result<(u64, String, tokio::fs::File)> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| rcgui_i18n::f("读取 {} 失败", &[&path.display()]))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = file
            .read(&mut buf)
            .await
            .with_context(|| rcgui_i18n::f("读取 {} 失败", &[&path.display()]))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > cap {
            bail!(
                "{}",
                rcgui_i18n::f("FILE_TOO_LARGE: {} 字节超过上限 {} 字节", &[&total, &cap])
            );
        }
        hasher.update(&buf[..n]);
    }
    if total != expect_len {
        bail!(
            "{}",
            rcgui_i18n::f("文件在挑选之后被改过，请重新选择：{}", &[&path.display()])
        );
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for byte in &digest[..16] {
        hex.push_str(&format!("{:02x}", byte));
    }
    file.seek(SeekFrom::Start(0))
        .await
        .with_context(|| rcgui_i18n::f("读取 {} 失败", &[&path.display()]))?;
    Ok((total, hex, file))
}

struct Part {
    name: String,
    value: String,
}

/// 校验过、可以直接发的一组参数。
struct Plan {
    url: String,
    method: reqwest::Method,
    headers: Vec<(String, String)>,
    /// 面板里那一栏（查授权表用）。
    form_key: String,
    /// multipart 里文件段的名字。
    part_name: String,
    file_name: Option<String>,
    mime: String,
    fields: Vec<Part>,
    idem_seed: Option<String>,
    file_cap: u64,
    timeout: Duration,
    reply_cap: usize,
}

impl Plan {
    fn parse(params: &Value) -> Result<Plan> {
        let raw_url = params
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let lower = raw_url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            bail!(
                "{}",
                rcgui_i18n::f(
                    "UPLOAD_BAD_PARAM: 地址必须是 http:// 或 https:// 开头：{}",
                    &[&redact(&raw_url)]
                )
            );
        }
        if has_ctl(&raw_url) || raw_url.contains(' ') {
            bail!(
                "{}",
                rcgui_i18n::f(
                    "UPLOAD_BAD_PARAM: 地址里不能有空白或换行：{}",
                    &[&redact(&raw_url)]
                )
            );
        }

        let method_text = params
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("POST")
            .trim()
            .to_ascii_uppercase();
        let method = match method_text.as_str() {
            "POST" => reqwest::Method::POST,
            "PUT" => reqwest::Method::PUT,
            "PATCH" => reqwest::Method::PATCH,
            other => bail!(
                "{}",
                rcgui_i18n::f("http.upload 不支持的方法：{}", &[&other])
            ),
        };

        let mut headers = Vec::new();
        if let Some(value) = params.get("headers") {
            if !value.is_null() {
                let Some(obj) = value.as_object() else {
                    bail!("{}", rcgui_i18n::t("UPLOAD_BAD_PARAM: headers 必须是对象"));
                };
                for (name, value) in obj {
                    let name = name.trim().to_ascii_lowercase();
                    if !is_token(&name) {
                        bail!(
                            "{}",
                            rcgui_i18n::f("HOST_HEADER: 请求头名不合法：{}", &[&clip(&name)])
                        );
                    }
                    if is_host_header(&name) {
                        bail!(
                            "{}",
                            rcgui_i18n::f(
                                "HOST_HEADER: 请求头 {} 由宿主决定，插件不能自己设",
                                &[&name]
                            )
                        );
                    }
                    let Some(text) = value.as_str() else {
                        bail!(
                            "{}",
                            rcgui_i18n::f("UPLOAD_BAD_PARAM: 请求头 {} 的值必须是字符串", &[&name])
                        );
                    };
                    if has_ctl(text) {
                        bail!(
                            "{}",
                            rcgui_i18n::f(
                                "UPLOAD_BAD_PARAM: 请求头 {} 的值里有换行或 NUL",
                                &[&name]
                            )
                        );
                    }
                    headers.push((name, text.to_string()));
                }
            }
        }

        let file = params.get("file");
        let part_name = file
            .and_then(|f| f.get("field"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let form_key = params
            .get("source")
            .and_then(|s| s.get("form_key"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let form_key = if form_key.is_empty() {
            part_name.clone()
        } else {
            form_key
        };
        if !is_key(&form_key) {
            bail!(
                "{}",
                rcgui_i18n::f("UPLOAD_BAD_PARAM: 表单字段名不合法：{}", &[&form_key])
            );
        }
        let part_name = if part_name.is_empty() {
            form_key.clone()
        } else {
            part_name
        };
        if !is_key(&part_name) {
            bail!(
                "{}",
                rcgui_i18n::f("UPLOAD_BAD_PARAM: 表单字段名不合法：{}", &[&part_name])
            );
        }
        let file_name = file
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .map(safe_file_name)
            .filter(|s| !s.is_empty());
        let mime = file
            .and_then(|f| f.get("mime"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| is_mime(s))
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let mut fields = Vec::new();
        match params.get("fields") {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) => {
                if items.len() > MAX_FIELDS {
                    bail!(
                        "{}",
                        rcgui_i18n::f("UPLOAD_BAD_PARAM: 表单字段最多 {} 个", &[&MAX_FIELDS])
                    );
                }
                for item in items {
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if !is_key(name) {
                        bail!(
                            "{}",
                            rcgui_i18n::f("UPLOAD_BAD_PARAM: 表单字段名不合法：{}", &[&name])
                        );
                    }
                    let value = match item.get("value") {
                        Some(Value::String(s)) => s.clone(),
                        None | Some(Value::Null) => String::new(),
                        Some(other) => other.to_string(),
                    };
                    if has_ctl(&value) {
                        bail!(
                            "{}",
                            rcgui_i18n::f(
                                "UPLOAD_BAD_PARAM: 表单字段 {} 的值里有换行或 NUL",
                                &[&name]
                            )
                        );
                    }
                    fields.push(Part {
                        name: name.to_string(),
                        value,
                    });
                }
            }
            Some(_) => bail!("{}", rcgui_i18n::t("UPLOAD_BAD_PARAM: fields 必须是数组")),
        }

        let idem_seed = params
            .get("idem_seed")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(seed) = &idem_seed {
            if !is_seed(seed) {
                bail!(
                    "{}",
                    rcgui_i18n::t("UPLOAD_BAD_PARAM: idem_seed 只能用字母、数字和 . _ : -")
                );
            }
        }

        // 两个名字的分歧是历史遗留：正式契约叫 `max_bytes`，而现有插件用
        // `max_file_bytes` 说文件上限、`max_bytes` 说回包上限。两个都认。
        let max_file_bytes = number(params, "max_file_bytes");
        let max_bytes = number(params, "max_bytes");
        let file_cap = match max_file_bytes.or(max_bytes) {
            Some(n) if n > 0 => n.min(MAX_FILE_CAP),
            _ => DEFAULT_FILE_CAP,
        };
        let reply_cap = match number(params, "max_body") {
            Some(n) if n > 0 => n.min(MAX_REPLY_CAP as u64) as usize,
            _ => match (max_file_bytes, max_bytes) {
                (Some(_), _) => DEFAULT_REPLY_CAP,
                (None, Some(n)) if n > 0 => n.min(MAX_REPLY_CAP as u64) as usize,
                _ => DEFAULT_REPLY_CAP,
            },
        };
        let timeout = Duration::from_millis(
            number(params, "timeout_ms")
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1_000, MAX_TIMEOUT_MS),
        );

        Ok(Plan {
            url: raw_url,
            method,
            headers,
            form_key,
            part_name,
            file_name,
            mime,
            fields,
            idem_seed,
            file_cap,
            timeout,
            reply_cap,
        })
    }
}

/// 数值参数：接受数字，也接受数字字符串（`config.json` 里的默认值常是字符串）。
fn number(params: &Value, key: &str) -> Option<u64> {
    match params.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// 这些头由宿主算或由传输层决定，插件设了就等于绕过大小、长度和身份约束。
fn is_host_header(name: &str) -> bool {
    matches!(
        name,
        "content-type"
            | "content-length"
            | "host"
            | "connection"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "expect"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-connection"
            | "keep-alive"
            | "cookie"
            | "set-cookie"
            | "idempotency-key"
    )
}

/// RFC 7230 的 token：请求头名只能是这些字符。
fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// 表单字段名：只认最普通的一批字符，免得名字里带引号把 multipart 拆坏。
fn is_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn is_seed(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn is_mime(s: &str) -> bool {
    let Some((kind, sub)) = s.split_once('/') else {
        return false;
    };
    !kind.is_empty()
        && !sub.is_empty()
        && kind.len() + sub.len() < 64
        && kind
            .bytes()
            .chain(sub.bytes())
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'.'))
}

fn has_ctl(s: &str) -> bool {
    s.chars().any(|c| c == '\r' || c == '\n' || c == '\0')
}

/// 文件名只留最后一段，去掉会破坏头部的字符；空结果由调用方回落到磁盘上的名字。
fn safe_file_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .to_string();
    base.chars()
        .filter(|c| !matches!(c, '"' | '\r' | '\n' | '\0') && !c.is_control())
        .take(200)
        .collect::<String>()
        .trim()
        .to_string()
}

/// 报错和日志里用的地址：去掉查询串、片段和 URL 里的用户名密码。
fn redact(url: &str) -> String {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once("://") else {
        return clip(url);
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, tail)) => {
            let path = tail.split(|c| c == '?' || c == '#').next().unwrap_or("");
            (authority, format!("/{}", path))
        }
        None => (
            rest.split(|c| c == '?' || c == '#').next().unwrap_or(rest),
            String::new(),
        ),
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let clean = format!("{}://{}{}", scheme, host, path);
    if clean == "://" {
        return clip(url);
    }
    clip(&clean)
}

fn clip(s: &str) -> String {
    let trimmed: String = s.chars().take(200).collect();
    if trimmed.chars().count() < s.chars().count() {
        format!("{}…", trimmed)
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// 一个只回固定响应的最小 HTTP 服务器：记下收到的原始字节和连接次数。
    struct Fake {
        url: String,
        hits: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<u8>>>,
    }

    fn fake(status: &str, headers: &str, body: &[u8]) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let reply = format!(
            "HTTP/1.1 {}\r\n{}Connection: close\r\nContent-Length: {}\r\n\r\n",
            status,
            headers,
            body.len()
        )
        .into_bytes();
        let mut reply_bytes = reply;
        reply_bytes.extend_from_slice(body);

        let hit_counter = hits.clone();
        let recorder = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                hit_counter.fetch_add(1, Ordering::SeqCst);
                let got = read_request(&mut stream);
                recorder.lock().unwrap().extend_from_slice(&got);
                let _ = stream.write_all(&reply_bytes);
                let _ = stream.flush();
            }
        });
        Fake {
            url: format!("http://{}/upload", addr),
            hits,
            seen,
        }
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// 按 Content-Length 把整个请求读完，这样断言看到的是完整的 multipart 体。
    fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0u8; 8192];
        let head_end = loop {
            let n = stream.read(&mut buf).unwrap_or(0);
            if n == 0 {
                return got;
            }
            got.extend_from_slice(&buf[..n]);
            if let Some(pos) = find(&got, b"\r\n\r\n") {
                break pos;
            }
        };
        let head = String::from_utf8_lossy(&got[..head_end]).to_ascii_lowercase();
        let want: usize = head
            .lines()
            .find_map(|line| {
                line.strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0);
        while got.len() < head_end + 4 + want {
            let n = stream.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        got
    }

    fn body_of(request: &[u8]) -> &[u8] {
        let start = find(request, b"\r\n\r\n").expect("没有头部结尾") + 4;
        &request[start..]
    }

    fn header_of(request: &[u8], name: &str) -> Option<String> {
        let end = find(request, b"\r\n\r\n")?;
        String::from_utf8_lossy(&request[..end])
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                if key.trim().eq_ignore_ascii_case(name) {
                    Some(value.trim().to_string())
                } else {
                    None
                }
            })
    }

    fn tmp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rcgui-upload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn params_for(url: &str) -> Value {
        json!({
            "url": url,
            "method": "POST",
            "headers": {"Authorization": "Bearer t"},
            "source": {"form_key": "file"},
            "file": {"field": "file", "name": "payload.bin", "mime": "application/octet-stream"},
            "fields": [
                {"name": "storage", "value": "s1"},
                {"name": "folderPath", "value": "a/b"}
            ],
            "idem_seed": "seed-a",
            "max_file_bytes": 1_048_576u64,
            "timeout_ms": 10_000,
            "max_bytes": 65_536
        })
    }

    #[tokio::test]
    async fn uploads_fields_then_file_with_digest() {
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let server = fake(
            "200 OK",
            "Content-Type: application/json\r\n",
            br#"{"ok":true}"#,
        );
        let path = tmp_file("happy.bin", &payload);
        grant("plug-a", "file", &path).unwrap();

        let out = upload("plug-a", &params_for(&server.url)).await.unwrap();
        assert_eq!(out["status"], 200);
        assert_eq!(out["size"], payload.len() as u64);
        assert_eq!(out["truncated"], false);
        assert_eq!(out["body"], r#"{"ok":true}"#);
        assert_eq!(server.hits.load(Ordering::SeqCst), 1);

        let request = server.seen.lock().unwrap().clone();
        assert!(
            request.starts_with(b"POST /upload HTTP/1.1"),
            "{}",
            String::from_utf8_lossy(&request[..40.min(request.len())])
        );
        assert_eq!(header_of(&request, "authorization").unwrap(), "Bearer t");

        let content_type = header_of(&request, "content-type").unwrap();
        let boundary = content_type
            .split("boundary=")
            .nth(1)
            .expect("缺 boundary")
            .to_string();
        assert!(content_type.starts_with("multipart/form-data; boundary=----rcgui"));

        let body = body_of(&request);
        // 宿主算的 Content-Length 必须和真正发出去的字节数一致。
        assert_eq!(
            header_of(&request, "content-length").unwrap(),
            body.len().to_string()
        );

        // 字段顺序：storage 在 folderPath 之前，两者都在文件段之前。
        let text = String::from_utf8_lossy(body).to_string();
        let storage = text.find("name=\"storage\"").unwrap();
        let folder = text.find("name=\"folderPath\"").unwrap();
        let file_part = text.find("name=\"file\"").unwrap();
        assert!(storage < folder && folder < file_part);
        assert!(text.contains("s1\r\n"));
        assert!(text.contains("a/b\r\n"));

        // 文件字节逐字节一致，尾部是收尾边界。
        let marker = b"name=\"file\"; filename=\"payload.bin\"";
        let marker_at = find(body, marker).unwrap();
        let bytes_at = find(&body[marker_at..], b"\r\n\r\n").unwrap() + marker_at + 4;
        let trailer = format!("\r\n--{}--\r\n", boundary).into_bytes();
        let bytes_end = find(body, &trailer).unwrap();
        assert_eq!(&body[bytes_at..bytes_end], &payload[..]);
        assert!(body.ends_with(&trailer));

        // 幂等键：种子 + 文件 sha256 的前 16 字节，插件自己算不出来。
        let mut hasher = Sha256::new();
        hasher.update(&payload);
        let digest = hasher.finalize();
        let want: String = digest[..16].iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(
            header_of(&request, "idempotency-key").unwrap(),
            format!("seed-a-{}", want)
        );
    }

    #[tokio::test]
    async fn refuses_without_grant_and_sends_nothing() {
        let server = fake("200 OK", "", b"{}");
        let path = tmp_file("nogrant.bin", b"never sent");
        // 授权是别的插件、别的栏位的，不能用。
        grant("plug-other", "file", &path).unwrap();

        let err = upload("plug-b", &params_for(&server.url))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("SOURCE_NOT_GRANTED"), "{}", err);
        assert_eq!(server.hits.load(Ordering::SeqCst), 0);
        assert!(server.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refuses_oversized_file_and_sends_nothing() {
        let server = fake("200 OK", "", b"{}");
        let payload = vec![7u8; 4096];
        let path = tmp_file("big.bin", &payload);
        grant("plug-c", "file", &path).unwrap();

        let mut params = params_for(&server.url);
        params["max_file_bytes"] = json!(1024);
        let err = upload("plug-c", &params).await.unwrap_err().to_string();
        assert!(
            err.contains("FILE_TOO_LARGE: 4096 字节超过上限 1024 字节"),
            "{}",
            err
        );
        assert_eq!(server.hits.load(Ordering::SeqCst), 0);
        assert!(server.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refuses_headers_the_host_owns() {
        let server = fake("200 OK", "", b"{}");
        let path = tmp_file("header.bin", b"x");
        grant("plug-d", "file", &path).unwrap();

        let mut params = params_for(&server.url);
        params["headers"] = json!({"Content-Type": "application/json"});
        let err = upload("plug-d", &params).await.unwrap_err().to_string();
        assert!(err.contains("HOST_HEADER: 请求头 content-type"), "{}", err);
        assert_eq!(server.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn refuses_unknown_scheme_and_sends_nothing() {
        let path = tmp_file("scheme.bin", b"x");
        grant("plug-e", "file", &path).unwrap();

        let mut params = params_for("file:///etc/passwd");
        params["source"] = json!({"form_key": "file"});
        let err = upload("plug-e", &params).await.unwrap_err().to_string();
        assert!(err.contains("UPLOAD_BAD_PARAM"), "{}", err);
    }

    #[tokio::test]
    async fn does_not_follow_redirects() {
        let server = fake(
            "302 Found",
            "Location: http://127.0.0.1:9/elsewhere\r\n",
            b"",
        );
        let path = tmp_file("redirect.bin", b"x");
        grant("plug-f", "file", &path).unwrap();

        let err = upload("plug-f", &params_for(&server.url))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("UPLOAD_REDIRECT"), "{}", err);
        // 只发了那一次：跳转目标没有被访问。
        assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn truncates_a_huge_reply() {
        let server = fake("200 OK", "", &vec![b'z'; 5000]);
        let path = tmp_file("reply.bin", b"x");
        grant("plug-g", "file", &path).unwrap();

        let mut params = params_for(&server.url);
        params["max_file_bytes"] = json!(1_048_576);
        params["max_body"] = json!(1024);
        let out = upload("plug-g", &params).await.unwrap();
        assert_eq!(out["truncated"], true);
        assert_eq!(out["body"].as_str().unwrap().len(), 1024);
    }

    #[tokio::test]
    async fn one_pick_authorises_one_send() {
        let server = fake("200 OK", "", b"{}");
        let path = tmp_file("once.bin", b"x");
        grant("plug-h", "file", &path).unwrap();

        upload("plug-h", &params_for(&server.url)).await.unwrap();
        let err = upload("plug-h", &params_for(&server.url))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("SOURCE_NOT_GRANTED"), "{}", err);
        assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refuses_file_that_changed_after_the_pick() {
        let server = fake("200 OK", "", b"{}");
        let path = tmp_file("changed.bin", b"original");
        grant("plug-i", "file", &path).unwrap();
        std::fs::write(&path, b"original but longer").unwrap();

        let err = upload("plug-i", &params_for(&server.url))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("文件在挑选之后被改过"), "{}", err);
        assert_eq!(server.hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn redacts_credentials_in_urls() {
        assert_eq!(
            redact("https://user:secret@example.com:8443/api/v1/upload?token=abc#x"),
            "https://example.com:8443/api/v1/upload"
        );
        assert_eq!(redact("http://127.0.0.1:9/up"), "http://127.0.0.1:9/up");
    }

    #[test]
    fn sanitises_file_names() {
        assert_eq!(safe_file_name("C:\\dir\\报告 \"1\".png"), "报告 1.png");
        assert_eq!(safe_file_name("a\r\nb.txt"), "ab.txt");
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
    }
}
