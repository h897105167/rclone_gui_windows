//! K-Vault 远程类型（`k-vault`）：rclone 的只读 WebDAV 后端 + 站点 HTTP API 上传。
//!
//! 为什么做成「webdav + 标记参数」而不是一个新的 rclone 后端：
//! - RCGUI 不内嵌 rclone，也不能给用户已装好的 rclone.exe 装后端插件，
//!   能复用的只有 rclone 自带的后端；
//! - K-Vault 站点已经把文件树暴露成**只读** WebDAV（`/dav`，只放行
//!   OPTIONS/PROPFIND/GET/HEAD），浏览与下载正好走 rclone 现成的通道；
//! - 写入（上传/删除/改名）在 DAV 侧一律 403，只能走站点 HTTP API，
//!   所以「上传」这一步由 GUI 直接发 `POST /api/v1/upload`，不经过 rclone。
//!
//! 远程参数的形状（`rclone config show <name>` 看到的）：
//!
//! ```text
//! [name]
//! type           = webdav
//! url            = https://host/dav
//! vendor         = other
//! bearer_token   = kvault_xxxxxxxxxx_yyyy      # 或 user/pass 基本认证
//! kvault         = 1                            # 标记：本 GUI 的 K-Vault 远程
//! kvault_folder  = image_bed                    # 上传目录（folderPath）
//! kvault_storage = telegram                     # 上传后端（storage）
//! kvault_dedup   = 1                            # 同名去重（deduplicate）
//! ```
//!
//! `kvault*` 这几个键 rclone 不认识，但实测会原样保存在配置文件里、且不影响
//! webdav 后端的构造（`config create` 后 `lsf` 能正常发出请求并返回站点的
//! 401 TOKEN_INVALID）。用它们做标记，就不必在 GUI 侧另建一套远程元数据。

use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::Duration;

/// 下拉框里的类型名（对用户可见，也是 `k-vault` 远程的唯一识别名）。
pub const TYPE: &str = "k-vault";
/// 落盘时实际使用的 rclone 后端类型：浏览/下载要靠它现成的只读支持。
pub const BACKEND: &str = "webdav";
/// 远程参数里的标记键：有它才是本 GUI 创建的 K-Vault 远程。
pub const MARKER: &str = "kvault";
/// `storage` 的合法取值（与站点文档一致）。
pub const STORAGES: [&str; 7] = [
    "telegram",
    "r2",
    "s3",
    "discord",
    "huggingface",
    "webdav",
    "github",
];
pub const DEFAULT_STORAGE: &str = "telegram";
/// 站点部署侧的单文件上限（100 MB）。超过就不读进内存，直接拒绝。
///
/// 令牌策略可能更小（实测交接令牌 15 MB），那种情况由服务端返回
/// `413 POLICY_SIZE_EXCEEDED`，提示里带的是策略值，比本地猜更可信。
pub const MAX_UPLOAD: u64 = 100 * 1024 * 1024;
/// GUI 侧明文令牌的命名空间（`<data>/plugins/kvault/kv.json`，键 = 远程名）。
///
/// 为什么令牌要另存一份明文：rclone 把 `bearer_token` 当密码，`config/dump`
/// 只能读回 **obscured** 串（老版本也没有 `config/reveal`），而站点 API 要的是
/// 明文。与其把 obscured 串当令牌发出去，不如在这里存一份；代价是它和
/// `config.json` 里的 RC 口令一样是明文落盘（既有债务，不是本次新增的设计）。
pub const STORE_NS: &str = "kvault";
/// 远程参数里令牌/口令的键（「已设置则不回显」判定用）。
pub const TOKEN_KEY: &str = "bearer_token";
pub const PASS_KEY: &str = "pass";

/// 一个 K-Vault 远程的配置视图（对应上面 `rclone config show` 的那组参数）。
#[derive(Clone, Debug, Default)]
pub struct KvCfg {
    /// 站点根地址，不含 `/dav`
    pub base: String,
    /// Bearer 令牌（`kvault_<id>_<secret>`）；空 = 用基本认证
    pub token: String,
    /// 基本认证用户名（DAV 只读通道用）
    pub user: String,
    /// 基本认证密码
    pub pass: String,
    /// 上传目录（`folderPath`）
    pub folder: String,
    /// 上传后端（`storage`）
    pub storage: String,
    /// 同名去重（`deduplicate=true`）
    pub dedup: bool,
}

/// `GET /api/v1/me` 里本模块关心的字段。
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub storages: Vec<String>,
    pub max_file: u64,
    pub folder_prefix: String,
}

/// 一次上传成功后的关键结果。
#[derive(Clone, Debug, Default)]
pub struct Uploaded {
    /// 站点记下的文件名（服务端可能改名，所以用返回值而不是本机名）。
    pub name: String,
    /// 签名直链（`links.download`）；站点在 MINIMIZE_KV_WRITES 下给的是
    /// `tgs_` 能力 URL，可直接分享，换 Telegram bot token 会失效。
    pub link: String,
}

impl KvCfg {
    /// 远程参数里是否带 `kvault=1` 标记。
    pub fn is_marked(params: &Map<String, Value>) -> bool {
        params
            .get(MARKER)
            .map(|v| truthy(v))
            .unwrap_or(false)
    }

    /// 从 `rclone config dump` 的**条目**（`Value::Object`）判定标记。
    pub fn is_marked_entry(entry: &Value) -> bool {
        entry.as_object().map(Self::is_marked).unwrap_or(false)
    }

    /// 参数里某个凭据键是否有值（不读出内容，只看存在与非空）。
    pub fn has_secret(params: &Map<String, Value>, key: &str) -> bool {
        params
            .get(key)
            .map(|v| match v {
                Value::String(s) => !s.trim().is_empty(),
                Value::Null => false,
                _ => true,
            })
            .unwrap_or(false)
    }

    /// 从 `rclone config dump` 的一个远程条目还原配置。
    ///
    /// `url` 里带着 `/dav` 尾巴，这里去掉：表单里显示的是站点根地址，
    /// 保存时再补回去，避免用户手填出 `/dav/dav`。
    ///
    /// **两个密码键不在这里还原**：`bearer_token` / `pass` 在 rclone 配置里是
    /// obscured 串，读回来当明文用会把认证搞坏（写回时还会被再 obscure 一次）。
    /// 令牌由 [`crate::kvault::STORE_NS`] 里的明文副本提供，口令由用户重新输入；
    /// 「是否已设置」用 [`KvCfg::has_secret`] 单独判断。
    pub fn from_params(params: &Map<String, Value>) -> KvCfg {
        let url = str_of(params, "url");
        let storage = {
            let s = str_of(params, "kvault_storage");
            if s.is_empty() {
                DEFAULT_STORAGE.to_string()
            } else {
                s
            }
        };
        KvCfg {
            base: base_from_url(&url),
            token: String::new(),
            user: str_of(params, "user"),
            pass: String::new(),
            folder: str_of(params, "kvault_folder"),
            storage,
            dedup: params
                .get("kvault_dedup")
                .map(|v| truthy(v))
                .unwrap_or(false),
        }
    }

    /// 反向：生成写进 rclone 的参数。
    ///
    /// `include_token=false` 用于「编辑时令牌留空」：此时**不能**写
    /// `bearer_token`（哪怕空串），否则会把已有令牌冲掉。
    pub fn params(&self, include_token: bool) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("url".into(), json!(self.dav_url()));
        // `vendor=other` 关掉 owncloud/nextcloud 那套私有探测：K-Vault 只是
        // 一个标准 WebDAV 文件树，站点也不返回 owncloud 的能力声明。
        m.insert("vendor".into(), json!("other"));
        let send_token = include_token && !self.token.is_empty();
        if send_token {
            m.insert("bearer_token".into(), json!(self.token));
        } else {
            // 没有令牌时才写基本认证。反过来：令牌一旦存在就优先用它，
            // 这也是 webdav 后端自己的判定顺序。
            if !self.user.is_empty() {
                m.insert("user".into(), json!(self.user));
            }
            if !self.pass.is_empty() {
                m.insert("pass".into(), json!(self.pass));
            }
        }
        m.insert(MARKER.into(), json!("1"));
        m.insert("kvault_folder".into(), json!(self.folder.trim().trim_matches('/')));
        m.insert("kvault_storage".into(), json!(self.storage));
        m.insert(
            "kvault_dedup".into(),
            json!(if self.dedup { "1" } else { "0" }),
        );
        m
    }

    /// rclone 侧要配的 WebDAV 地址。
    pub fn dav_url(&self) -> String {
        format!("{}/dav", normalize_base(&self.base))
    }

    /// 上传是否具备最小条件（有站点地址 + 令牌）。返回 `Err` 时是给用户看的中文原因。
    pub fn upload_ready(&self) -> Result<(), String> {
        if self.base.trim().is_empty() {
            return Err("K-Vault 远程没有站点地址".into());
        }
        if self.token.trim().is_empty() {
            return Err(
                "K-Vault 上传需要 API 访问令牌（WebDAV 通道只读）。请编辑该远程填入令牌。".into(),
            );
        }
        Ok(())
    }
}

// ---------------- HTTP ----------------

fn http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))
}

/// 探活 + 读令牌信息：`GET /api/health` 判站点是否在，`GET /api/v1/me` 判令牌与策略。
///
/// 两步分开的原因：站点不在和令牌不对是两种完全不同的故障，合成一句
/// 「连接失败」会让用户去改令牌，实际是域名/网络的问题。
pub async fn probe(base: &str, token: &str) -> Result<Probe, String> {
    let base = normalize_base(base);
    if base.is_empty() {
        return Err("站点地址不能为空".into());
    }
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err("站点地址要以 http:// 或 https:// 开头".into());
    }
    let http = http_client(Duration::from_secs(15))?;

    let health = http
        .get(format!("{base}/api/health"))
        .send()
        .await
        .map_err(|e| format!("站点不可达: {e}"))?;
    let status = health.status().as_u16();
    let body = health.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(format!("健康检查失败 · {}", api_error(status, &body)));
    }

    if token.trim().is_empty() {
        // 没填令牌也能算「站点可用」：表单允许只配基本认证做浏览。
        return Ok(Probe::default());
    }
    let me = http
        .get(format!("{base}/api/v1/me"))
        .bearer_auth(token.trim())
        .send()
        .await
        .map_err(|e| format!("读取令牌信息失败: {e}"))?;
    let status = me.status().as_u16();
    let body = me.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(format!("令牌不可用 · {}", api_error(status, &body)));
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("令牌信息不是 JSON（HTTP {status}）: {e}"))?;
    Ok(parse_me(&v))
}

/// 解析 `GET /api/v1/me`。
///
/// 站点实际返回的是 `{"success":true,"data":{"token":{…}}}`（2026-10-23 实测），
/// 交接文档里另外写过平铺形式；三种位置都试一遍，免得取值路径不对又被当成「没填令牌」。
fn parse_me(v: &Value) -> Probe {
    let data = v.get("data").filter(|d| d.is_object());
    let node = data
        .and_then(|d| d.get("token"))
        .or_else(|| v.get("token"))
        .or(data)
        .cloned()
        .unwrap_or_else(|| v.clone());
    let policies = node
        .get("policies")
        .or_else(|| data.and_then(|d| d.get("policies")))
        .or_else(|| v.get("policies"))
        .cloned()
        .unwrap_or(json!({}));
    let strings = |v: Option<&Value>| -> Vec<String> {
        v.and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    Probe {
        id: node.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        name: node
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        scopes: strings(node.get("scopes")),
        storages: strings(policies.get("allowedStorages")),
        max_file: policies
            .get("maxFileSize")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        folder_prefix: policies
            .get("folderPrefix")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

/// 上传一个文件到 `folder`，`name` 作为最终文件名（同名处理由调用方完成）。
///
/// 为什么整文件读进内存：站点按 multipart 收单个 `file` 段，reqwest 的
/// `Part::file` 需要 `stream` feature，而这里的上限就是 100 MB（站点侧），
/// 读一次内存换来不引入新 feature，是可接受的取舍。
pub async fn upload(cfg: &KvCfg, path: &Path, name: &str, folder: &str) -> Result<Uploaded, String> {
    cfg.upload_ready()?;
    let meta = std::fs::metadata(path).map_err(|e| format!("读取文件失败: {e}"))?;
    if meta.len() > MAX_UPLOAD {
        return Err(format!(
            "文件 {:.1} MB 超过站点单文件上限 {:.0} MB",
            meta.len() as f64 / 1048576.0,
            MAX_UPLOAD as f64 / 1048576.0
        ));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("读取文件失败: {e}"))?;
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(name.to_string())
        .mime_str(mime_for(name))
        .map_err(|e| format!("构造上传请求失败: {e}"))?;
    let mut form = reqwest::multipart::Form::new().part("file", part);
    // storage 必须显式给：该部署的令牌策略是 fail-closed 的，缺了直接
    // `403 POLICY_STORAGE_DENIED`，不会替你挑默认后端。
    form = form.text("storage", cfg.storage.clone());
    let folder = folder.trim().trim_matches('/');
    if !folder.is_empty() {
        form = form.text("folderPath", folder.to_string());
    }
    if cfg.dedup {
        form = form.text("deduplicate", "true");
    }

    // 上传超时给足：15 MB 的图在慢上行链路上也可能要几十秒。
    let http = http_client(Duration::from_secs(300))?;
    let resp = http
        .post(format!("{}/api/v1/upload", normalize_base(&cfg.base)))
        .bearer_auth(cfg.token.trim())
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("上传请求失败: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(api_error(status, &body));
    }
    let v: Value =
        serde_json::from_str(&body).map_err(|e| format!("上传响应不是 JSON: {e}"))?;
    let file = v.get("file").cloned().unwrap_or(json!({}));
    let link = v
        .get("links")
        .and_then(|l| l.get("download"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    Ok(Uploaded {
        name: file
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or(name)
            .to_string(),
        link: absolutize(&cfg.base, link),
    })
}

/// 把 `{"success":false,"error":{"code","message"}}` 摊平成一行中文，
/// 认不出来就退回状态码 + 正文前 160 字（Cloudflare 的错误页也是正文）。
fn api_error(status: u16, body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(err) = v.get("error") {
            let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("");
            let msg = err.get("message").and_then(|c| c.as_str()).unwrap_or("");
            if !code.is_empty() {
                return format!("HTTP {status} · {code}: {msg}");
            }
        }
    }
    let snippet: String = body.trim().chars().take(160).collect();
    if snippet.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {snippet}")
    }
}

// ---------------- 小工具 ----------------

/// 去掉尾部斜杠与 `/dav`，得到站点根地址。
pub fn base_from_url(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix("/dav").unwrap_or(u);
    u.trim_end_matches('/').to_string()
}

fn normalize_base(base: &str) -> String {
    base_from_url(base.trim())
}

/// 响应里的直链可能被写成相对路径（取决于站点实现），统一补成绝对地址。
fn absolutize(base: &str, link: &str) -> String {
    if link.is_empty() {
        return String::new();
    }
    if link.starts_with("http://") || link.starts_with("https://") {
        return link.to_string();
    }
    format!("{}/{}", normalize_base(base), link.trim_start_matches('/'))
}

fn str_of(p: &Map<String, Value>, key: &str) -> String {
    p.get(key)
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => matches!(s.trim(), "1" | "true" | "yes" | "on"),
        _ => false,
    }
}

/// 上传时的 Content-Type。站点的 MIME 策略只看这一层，猜错会被
/// `POLICY_MIME_DENIED` 拒掉，所以常见类型都按扩展名写全。
fn mime_for(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "txt" => "text/plain",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Map<String, Value> {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), json!(v));
        }
        m
    }

    #[test]
    fn marker_requires_kvault_key() {
        assert!(!KvCfg::is_marked(&params(&[("type", "webdav")])));
        assert!(KvCfg::is_marked(&params(&[("kvault", "1")])));
        assert!(!KvCfg::is_marked(&params(&[("kvault", "0")])));
    }

    #[test]
    fn round_trip_params() {
        let cfg = KvCfg {
            base: "https://kvault.example.com/".into(),
            token: "kvault_abc_def".into(),
            user: String::new(),
            pass: String::new(),
            folder: "/image_bed/".into(),
            storage: "telegram".into(),
            dedup: true,
        };
        let p = cfg.params(true);
        assert_eq!(p.get("url").unwrap().as_str().unwrap(), "https://kvault.example.com/dav");
        assert_eq!(p.get("vendor").unwrap().as_str().unwrap(), "other");
        assert_eq!(p.get("kvault_folder").unwrap().as_str().unwrap(), "image_bed");
        assert_eq!(p.get("kvault_dedup").unwrap().as_str().unwrap(), "1");

        let back = KvCfg::from_params(&p);
        assert_eq!(back.base, "https://kvault.example.com");
        // 令牌不参与回读：rclone 里存的是 obscured 串
        assert_eq!(back.token, "");
        assert!(KvCfg::has_secret(&p, TOKEN_KEY));
        assert_eq!(back.folder, "image_bed");
        assert!(back.dedup);
        assert!(KvCfg::is_marked(&p));
    }

    #[test]
    fn has_secret_ignores_blank_values() {
        assert!(!KvCfg::has_secret(&params(&[("pass", "")]), PASS_KEY));
        assert!(!KvCfg::has_secret(&params(&[]), PASS_KEY));
        assert!(KvCfg::has_secret(&params(&[("pass", "x")]), PASS_KEY));
        // 不是字符串也算「已设置」：rclone 偶尔把布尔写进来
        let mut m = Map::new();
        m.insert("pass".into(), json!(true));
        assert!(KvCfg::has_secret(&m, PASS_KEY));
    }

    #[test]
    fn token_left_blank_does_not_overwrite() {
        // 编辑时令牌留空：不得写 bearer_token，也不该顺手写空口令
        let cfg = KvCfg {
            base: "https://kvault.example.com".into(),
            token: String::new(),
            user: String::new(),
            pass: String::new(),
            ..Default::default()
        };
        let p = cfg.params(false);
        assert!(p.get("bearer_token").is_none());
        assert!(p.get("user").is_none());
        assert!(p.get("pass").is_none());
    }

    #[test]
    fn basic_auth_used_when_no_token() {
        let cfg = KvCfg {
            base: "https://kvault.example.com".into(),
            user: "admin".into(),
            pass: "secret".into(),
            ..Default::default()
        };
        let p = cfg.params(true);
        assert!(p.get("bearer_token").is_none());
        assert_eq!(p.get("user").unwrap().as_str().unwrap(), "admin");
        assert_eq!(p.get("pass").unwrap().as_str().unwrap(), "secret");
    }

    #[test]
    fn base_strips_dav_once() {
        assert_eq!(base_from_url("https://h/dav"), "https://h");
        assert_eq!(base_from_url("https://h/dav/"), "https://h");
        assert_eq!(base_from_url("https://h/"), "https://h");
        assert_eq!(base_from_url("https://h/dav/dav"), "https://h/dav");
        assert_eq!(KvCfg {
            base: "https://h".into(),
            ..Default::default()
        }
        .dav_url(), "https://h/dav");
    }

    #[test]
    fn error_envelope_is_flattened() {
        let body = r#"{"success":false,"error":{"code":"POLICY_STORAGE_DENIED","message":"API Token policy only allows storage: telegram."}}"#;
        let msg = api_error(403, body);
        assert!(msg.contains("POLICY_STORAGE_DENIED"));
        assert!(msg.contains("only allows storage"));
        // 非 JSON（例如 CF 的 HTML 错误页）不能整段糊上去
        let html = format!("<html>{}</html>", "x".repeat(400));
        let msg = api_error(502, &html);
        assert!(msg.len() < 220);
    }

    #[test]
    fn upload_requires_token() {
        let cfg = KvCfg {
            base: "https://h".into(),
            ..Default::default()
        };
        assert!(cfg.upload_ready().is_err());
    }

    #[test]
    fn mime_by_extension() {
        assert_eq!(mime_for("a.PNG"), "image/png");
        assert_eq!(mime_for("a.webm"), "application/octet-stream");
        assert_eq!(mime_for("noext"), "application/octet-stream");
    }

    /// 站点真实返回（交接文档 2026-10-23 实测）：`data.token` 包一层。
    /// 曾经只认平铺字段，导致填好令牌还提示「未填令牌」。
    #[test]
    fn me_is_read_from_data_token() {
        let v: Value = serde_json::from_str(
            r#"{"success":true,"data":{"token":{"id":"EVhBVOYTA6bP","name":"图床",
            "scopes":["upload","read"],"enabled":true,
            "policies":{"allowedStorages":["telegram"],"maxFileSize":15728640,
            "folderPrefix":"image_bed"}}}}"#,
        )
        .unwrap();
        let p = parse_me(&v);
        assert_eq!(p.id, "EVhBVOYTA6bP");
        assert_eq!(p.name, "图床");
        assert_eq!(p.scopes, vec!["upload", "read"]);
        assert_eq!(p.storages, vec!["telegram"]);
        assert_eq!(p.max_file, 15728640);
        assert_eq!(p.folder_prefix, "image_bed");
    }

    /// 平铺形式仍然要能读，免得站点回退旧格式时又坏掉。
    #[test]
    fn me_is_read_from_flat_body() {
        let v: Value = serde_json::from_str(
            r#"{"token":{"id":"abc","scopes":["read"]},
                "policies":{"allowedStorages":["r2"],"maxFileSize":1024,"folderPrefix":""}}"#,
        )
        .unwrap();
        let p = parse_me(&v);
        assert_eq!(p.id, "abc");
        assert!(p.folder_prefix.is_empty());
        assert_eq!(p.max_file, 1024);
    }
}
