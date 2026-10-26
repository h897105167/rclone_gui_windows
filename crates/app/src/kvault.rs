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
    /// 站点判定「内容已存在」，直接复用了老文件的直链（只有开了 deduplicate
    /// 且文件 ≤ 25 MB 时才会命中，见站点 `functions/api/v1/upload.js`）。
    pub dedup: bool,
}

/// 站点索引里的一条文件记录（`GET /api/v1/files` 的一项）。
///
/// 和 WebDAV 的 PROPFIND 相比，这份索引**不经过 5 分钟的目录树缓存**，
/// 所以刚上传的文件在这里立刻能看到；删除要用的 `id` 也只有这里有。
#[derive(Clone, Debug, Default)]
pub struct RemoteFile {
    /// KV 主键（`tg:…` / `r2:…` / `s3:…`），`DELETE /api/v1/file/<id>` 用它。
    pub id: String,
    /// 站点记下的原始文件名。
    pub name: String,
    /// 所在目录（`folderPath`，无首尾斜杠；根目录为空串）。
    pub folder: String,
    pub size: u64,
    /// 上传时间（`uploadedAt`，RFC3339）。
    pub uploaded: String,
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
            return Err(rcgui_i18n::t("K-Vault 远程没有站点地址").into());
        }
        if self.token.trim().is_empty() {
            return Err(
                rcgui_i18n::t("K-Vault 上传需要 API 访问令牌（WebDAV 通道只读）。请编辑该远程填入令牌。").into(),
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
        .map_err(|e| rcgui_i18n::f("创建 HTTP 客户端失败: {}", &[&(e)]))
}

/// 探活 + 读令牌信息：`GET /api/health` 判站点是否在，`GET /api/v1/me` 判令牌与策略。
///
/// 两步分开的原因：站点不在和令牌不对是两种完全不同的故障，合成一句
/// 「连接失败」会让用户去改令牌，实际是域名/网络的问题。
pub async fn probe(base: &str, token: &str) -> Result<Probe, String> {
    let base = normalize_base(base);
    if base.is_empty() {
        return Err(rcgui_i18n::t("站点地址不能为空").into());
    }
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(rcgui_i18n::t("站点地址要以 http:// 或 https:// 开头").into());
    }
    let http = http_client(Duration::from_secs(15))?;

    let health = http
        .get(format!("{base}/api/health"))
        .send()
        .await
        .map_err(|e| rcgui_i18n::f("站点不可达: {}", &[&(e)]))?;
    let status = health.status().as_u16();
    let body = health.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(rcgui_i18n::f("健康检查失败 · {}", &[&(api_error(status, &body))]));
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
        .map_err(|e| rcgui_i18n::f("读取令牌信息失败: {}", &[&(e)]))?;
    let status = me.status().as_u16();
    let body = me.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(rcgui_i18n::f("令牌不可用 · {}", &[&(api_error(status, &body))]));
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| rcgui_i18n::f("令牌信息不是 JSON（HTTP {}）: {}", &[&(status), &(e)]))?;
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
///
/// 带 `Idempotency-Key`：站点对同一把钥匙在 24 小时内只存一份，网络抖动重试
/// 不会变成两个文件（站点实现见 `functions/api/v1/upload.js`）。钥匙把「目标
/// 目录 + 后端 + 文件名 + 大小 + 秒级 mtime」都算进去，所以「同一份文件传到
/// 另一个目录」不会误命中上一次的响应。
pub async fn upload(cfg: &KvCfg, path: &Path, name: &str, folder: &str) -> Result<Uploaded, String> {
    cfg.upload_ready()?;
    let meta = std::fs::metadata(path).map_err(|e| rcgui_i18n::f("读取文件失败: {}", &[&(e)]))?;
    if meta.len() > MAX_UPLOAD {
        return Err(rcgui_i18n::f("文件 {} MB 超过站点单文件上限 {} MB", &[&(format!("{:.1}", meta.len() as f64 / 1048576.0)), &(format!("{:.0}", MAX_UPLOAD as f64 / 1048576.0))]));
    }
    let folder = folder.trim().trim_matches('/').to_string();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let idem = idem_key(&cfg.base, &cfg.storage, &folder, name, meta.len(), mtime);

    // 只重试一次：站点偶发 HTTP/2 建连失败（Cloudflare 侧），第二次几乎必成。
    // 真正的业务错误（403/413）不重试，重试只会把同一句错误再说一遍。
    let mut last = String::new();
    for attempt in 0..2 {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| rcgui_i18n::f("读取文件失败: {}", &[&(e)]))?;
        match post_upload(cfg, bytes, name, &folder, &idem).await {
            Ok(u) => return Ok(u),
            Err((retry, msg)) => {
                last = msg;
                if !retry || attempt == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
        }
    }
    Err(last)
}

/// 幂等键：同一份文件 + 同一个目标位置 → 同一把钥匙。
fn idem_key(base: &str, storage: &str, folder: &str, name: &str, size: u64, mtime: u64) -> String {
    let seed = format!("{base}|{storage}|{folder}|{name}|{size}|{mtime}");
    format!("rcgui-{:016x}", fnv1a(&seed))
}

/// FNV-1a 64 位：只为得到稳定短键，不做安全用途。
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 发一次上传请求。`Err((是否值得重试, 给用户看的中文))`。
async fn post_upload(
    cfg: &KvCfg,
    bytes: Vec<u8>,
    name: &str,
    folder: &str,
    idem: &str,
) -> Result<Uploaded, (bool, String)> {
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(name.to_string())
        .mime_str(mime_for(name))
        .map_err(|e| (false, rcgui_i18n::f("构造上传请求失败: {}", &[&(e)])))?;
    let mut form = reqwest::multipart::Form::new().part("file", part);
    // storage 必须显式给：该部署的令牌策略是 fail-closed 的，缺了直接
    // `403 POLICY_STORAGE_DENIED`，不会替你挑默认后端。
    form = form.text("storage", cfg.storage.clone());
    if !folder.is_empty() {
        form = form.text("folderPath", folder.to_string());
    }
    if cfg.dedup {
        form = form.text("deduplicate", "true");
    }

    // 上传超时给足：15 MB 的图在慢上行链路上也可能要几十秒。
    let http = match http_client(Duration::from_secs(300)) {
        Ok(c) => c,
        Err(e) => return Err((false, e)),
    };
    let resp = match http
        .post(format!("{}/api/v1/upload", normalize_base(&cfg.base)))
        .bearer_auth(cfg.token.trim())
        .header("Idempotency-Key", idem)
        .multipart(form)
        .send()
        .await
    {
        Ok(r) => r,
        // 连接层面的失败才值得重试；`is_body`/`is_decode` 这类是确定性的。
        Err(e) => {
            let retry = e.is_timeout() || e.is_connect() || e.is_request();
            return Err((retry, rcgui_i18n::f("上传请求失败: {}", &[&(e)])));
        }
    };
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if status >= 400 {
        // 429 是令牌限流，5xx 是站点/Cloudflare 抖动，都算「过一会儿再来」。
        let retry = status == 429 || status >= 500;
        return Err((retry, api_error(status, &body)));
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| (false, rcgui_i18n::f("上传响应不是 JSON: {}", &[&(e)])))?;
    // 站点的成功信封是 `{success, file, links, deduplicated?}`；老文档写过
    // 平铺形式，`data` 里也照看一眼。
    let root = v.get("data").filter(|d| d.is_object()).unwrap_or(&v);
    let file = root.get("file").cloned().unwrap_or(json!({}));
    let link = root
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
        dedup: truthy(root.get("deduplicated").unwrap_or(&Value::Null)),
    })
}

// ---------------- 站点索引（列表 / 删除） ----------------

/// 列表每页条数。站点没写上限，管理页用的是 50；这里取大一点少跑几轮。
const LIST_PAGE: usize = 200;
/// 翻页上限：防止索引异常时无限翻（200 × 25 = 5000 条）。
const LIST_MAX_PAGES: usize = 25;

/// 读站点索引 `GET /api/v1/files`（自动翻页）。
///
/// 为什么要读这份索引而不是全信 WebDAV：站点的 DAV 目录树有一层 5 分钟的
/// KV 缓存（`dav:tree:v2`，上传/删除都不会主动失效），而这份索引直读 KV，
/// 刚传完的文件立刻可见，删除要用的文件 id 也只有这里有。
pub async fn list_files(base: &str, token: &str) -> Result<Vec<RemoteFile>, String> {
    let base = normalize_base(base);
    if base.is_empty() {
        return Err(rcgui_i18n::t("站点地址不能为空").into());
    }
    if token.trim().is_empty() {
        return Err(rcgui_i18n::t("读取站点文件列表需要 API 令牌（令牌要有 read 权限）").into());
    }
    let http = http_client(Duration::from_secs(30))?;
    let mut out: Vec<RemoteFile> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..LIST_MAX_PAGES {
        let mut req = http
            .get(format!("{base}/api/v1/files"))
            .query(&[("limit", LIST_PAGE.to_string())])
            .bearer_auth(token.trim());
        if let Some(c) = &cursor {
            req = req.query(&[("cursor", c.clone())]);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| rcgui_i18n::f("读取站点文件列表失败: {}", &[&(e)]))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if status >= 400 {
            return Err(rcgui_i18n::f("读取站点文件列表失败 · {}", &[&(api_error(status, &body))]));
        }
        let v: Value = serde_json::from_str(&body)
            .map_err(|e| rcgui_i18n::f("文件列表不是 JSON（HTTP {}）: {}", &[&(status), &(e)]))?;
        let (page, next) = parse_files(&v);
        out.extend(page);
        match next {
            Some(c) if !c.is_empty() => cursor = Some(c),
            _ => return Ok(out),
        }
    }
    Ok(out)
}

/// 解析一页 `GET /api/v1/files`，返回（本页记录，下一页 cursor）。
fn parse_files(v: &Value) -> (Vec<RemoteFile>, Option<String>) {
    let root = v.get("data").filter(|d| d.is_object()).unwrap_or(v);
    let files = root
        .get("files")
        .or_else(|| v.get("files"))
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let list = files
        .iter()
        .map(|f| RemoteFile {
            id: f.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            name: f
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            folder: f
                .get("folderPath")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .trim_matches('/')
                .to_string(),
            size: f.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
            uploaded: f
                .get("uploadedAt")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .filter(|f| !f.id.is_empty())
        .collect();
    let pagination = root.get("pagination").or_else(|| v.get("pagination"));
    let complete = pagination
        .and_then(|p| p.get("listComplete"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let next = if complete {
        None
    } else {
        pagination
            .and_then(|p| p.get("cursor"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
    };
    (list, next)
}

/// 删一个文件：`DELETE /api/v1/file/<id>`（要 `delete` 权限）。
pub async fn delete_file(base: &str, token: &str, id: &str) -> Result<(), String> {
    let base = normalize_base(base);
    if base.is_empty() {
        return Err(rcgui_i18n::t("站点地址不能为空").into());
    }
    if token.trim().is_empty() {
        return Err(rcgui_i18n::t("删除站点文件需要 API 令牌（令牌要有 delete 权限）").into());
    }
    let http = http_client(Duration::from_secs(60))?;
    let resp = http
        .delete(format!("{base}/api/v1/file/{}", encode_id(id)))
        .bearer_auth(token.trim())
        .send()
        .await
        .map_err(|e| rcgui_i18n::f("删除请求失败: {}", &[&(e)]))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(api_error(status, &body));
    }
    Ok(())
}

/// 文件 id（KV 主键，形如 `tg:AgACAg…` / `r2:dir/a.png`）编成 URL 路径段。
///
/// 保留 `/` 和 `:`：两者在路径段里都是合法字符，站点也是按解码后的整串去
/// 查主键的（`decodeURIComponent(params.id)`），编得越少越不容易踩到歧义。
fn encode_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.as_bytes() {
        let ch = *b as char;
        let keep = ch.is_ascii_alphanumeric()
            || matches!(ch, '-' | '_' | '.' | '~' | ':' | '/');
        if keep {
            out.push(ch);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 站点给重名文件补的 DAV 显示后缀本体（`a~ab12cd.txt` → `ab12cd`）。
///
/// 后缀就是文件主键的末 6 位，所以它同时也是「在 GUI 里点的到底是哪一条」的
/// 唯一线索——同目录同名文件靠它区分。
fn collision_tail(name: &str) -> Option<&str> {
    let base = match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    };
    let tilde = base.rfind('~')?;
    let tail = &base[tilde + 1..];
    (tail.len() == 6 && tail.chars().all(|c| c.is_ascii_alphanumeric())).then_some(tail)
}

/// 去掉站点给重名文件加的 DAV 显示后缀（`a~ab12cd.txt` → `a.txt`）。
///
/// 站点的只读 DAV 在同一个目录里遇到同名文件时，会给后来者补一个
/// `~<主键末 6 位>`，只在 PROPFIND 的显示名里出现；索引里的文件名是原始的。
/// 拿 DAV 的显示名去索引里找文件，得先把这个后缀摘掉。
pub fn strip_collision_suffix(name: &str) -> String {
    let Some(tail) = collision_tail(name) else {
        return name.to_string();
    };
    let marker = format!("~{tail}");
    match name.find(&marker) {
        Some(i) => format!("{}{}", &name[..i], &name[i + marker.len()..]),
        None => name.to_string(),
    }
}

/// 在站点索引里找出「目录 `folder` 下的 `name`」对应的文件 id。
///
/// 目录（`is_dir=true`）返回其下所有文件的 id；文件返回匹配项的 id
/// （可能不止一条：站点允许同目录同名，调用方自己决定怎么处理）。
///
/// 带 DAV 后缀的显示名（`a~ff0011.png`）优先按「主键末 6 位」定位：这正是
/// 后缀的来源，能唯一定位时就不会误伤同名的另一个文件。
pub fn pick_targets(files: &[RemoteFile], folder: &str, name: &str, is_dir: bool) -> Vec<String> {
    let dir = folder.trim().trim_matches('/');
    if is_dir {
        let self_dir = if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        };
        let under = format!("{self_dir}/");
        return files
            .iter()
            .filter(|f| f.folder == self_dir || f.folder.starts_with(&under))
            .map(|f| f.id.clone())
            .collect();
    }
    let want = strip_collision_suffix(name);
    let candidates: Vec<&RemoteFile> = files
        .iter()
        .filter(|f| f.folder == dir)
        .filter(|f| f.name == name || f.name == want)
        .collect();
    if let Some(tail) = collision_tail(name) {
        let exact: Vec<String> = candidates
            .iter()
            .filter(|f| f.id.ends_with(tail))
            .map(|f| f.id.clone())
            .collect();
        if exact.len() == 1 {
            return exact;
        }
    }
    candidates.iter().map(|f| f.id.clone()).collect()
}

/// 文件列表里的一行（站点索引按目录切出来的结果）。
#[derive(Clone, Debug, Default)]
pub struct KvItem {
    pub name: String,
    /// 由索引里的 `folderPath` 前缀推出来的子目录（站点没有真正的目录对象，
    /// 也不会为「空目录」留记录）。
    pub is_dir: bool,
    pub size: u64,
    /// `uploadedAt`（RFC3339）。字典序即时间序，和 rclone 的 `mod_time` 一致。
    pub uploaded: String,
}

/// 把站点索引切成「`folder` 这一层」的条目，并按 GUI 的排序规则排好。
///
/// 排序规则和 `main.rs::file_rows` 保持一致：目录永远在前，同类按
/// `sort_key`（`name` / `size` / `mtime`）排，`desc` 反向。目录的大小按 0 算
/// （站点没有目录大小），时间取该目录下最新的一条，这样按时间倒序时，
/// 最近传过东西的目录会冒到前面。
pub fn list_folder(files: &[RemoteFile], folder: &str, sort_key: &str, desc: bool) -> Vec<KvItem> {
    let dir = folder.trim().trim_matches('/');
    let mut dirs: Vec<KvItem> = Vec::new();
    let mut items: Vec<KvItem> = Vec::new();
    for f in files {
        if f.folder == dir {
            items.push(KvItem {
                name: f.name.clone(),
                is_dir: false,
                size: f.size,
                uploaded: f.uploaded.clone(),
            });
            continue;
        }
        // 只认「当前目录下面一层」：更深的路径只贡献它的第一个路径段
        let rest = if dir.is_empty() {
            f.folder.as_str()
        } else if let Some(r) = f.folder.strip_prefix(&format!("{dir}/")) {
            r
        } else {
            continue;
        };
        let seg = rest.split('/').next().unwrap_or(rest);
        if seg.is_empty() {
            continue;
        }
        match dirs.iter_mut().find(|d| d.name == seg) {
            Some(d) => {
                if f.uploaded > d.uploaded {
                    d.uploaded = f.uploaded.clone();
                }
            }
            None => dirs.push(KvItem {
                name: seg.to_string(),
                is_dir: true,
                size: 0,
                uploaded: f.uploaded.clone(),
            }),
        }
    }
    let by = |a: &KvItem, b: &KvItem| -> std::cmp::Ordering {
        let ord = if sort_key == "size" {
            a.size.cmp(&b.size).then_with(|| a.name.cmp(&b.name))
        } else if sort_key == "mtime" {
            a.uploaded
                .cmp(&b.uploaded)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        } else {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        };
        if desc {
            ord.reverse()
        } else {
            ord
        }
    };
    dirs.sort_by(by);
    items.sort_by(by);
    dirs.extend(items);
    dirs
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

    /// 造一条站点索引记录（测试用）。
    fn rf(id: &str, name: &str, folder: &str, size: u64, uploaded: &str) -> RemoteFile {
        RemoteFile {
            id: id.to_string(),
            name: name.to_string(),
            folder: folder.to_string(),
            size,
            uploaded: uploaded.to_string(),
        }
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

    #[test]
    fn files_page_is_parsed_with_cursor() {
        let v: Value = serde_json::from_str(
            r#"{"success":true,"files":[
                {"id":"tg:AgAC","name":"a.png","size":10,"folderPath":"image_bed"},
                {"id":"r2:dir/b.txt","name":"b.txt","size":20,"folderPath":"/dir/"}],
                "pagination":{"cursor":"next-1","listComplete":false,"pageCount":2,"total":3}}"#,
        )
        .unwrap();
        let (files, next) = parse_files(&v);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].folder, "image_bed");
        // folderPath 前后的斜杠要被吃掉，否则匹配不上 GUI 路径
        assert_eq!(files[1].folder, "dir");
        assert_eq!(next.as_deref(), Some("next-1"));

        let done: Value = serde_json::from_str(
            r#"{"files":[],"pagination":{"cursor":null,"listComplete":true}}"#,
        )
        .unwrap();
        assert!(parse_files(&done).1.is_none());
    }

    #[test]
    fn collision_suffix_is_stripped() {
        assert_eq!(strip_collision_suffix("a~ab12cd.txt"), "a.txt");
        assert_eq!(strip_collision_suffix("照片~0011aa.png"), "照片.png");
        // 正常的波浪号/短尾不能被误摘
        assert_eq!(strip_collision_suffix("a~b.txt"), "a~b.txt");
        assert_eq!(strip_collision_suffix("a~toolong.txt"), "a~toolong.txt");
        assert_eq!(strip_collision_suffix("plain"), "plain");
    }

    #[test]
    fn targets_match_folder_and_name() {
        let files = vec![
            rf("tg:AAAAAA", "a.png", "image_bed", 1, "2026-10-23T10:00:00Z"),
            rf("tg:00ff0011", "a.png", "image_bed", 1, "2026-10-23T10:00:00Z"),
            rf("tg:3", "b.png", "image_bed/sub", 1, "2026-10-23T10:00:00Z"),
            rf("tg:4", "a.png", "other", 1, "2026-10-23T10:00:00Z"),
        ];
        // 同目录两个同名文件：索引里都叫 a.png，点哪个都说不清，两个都返回，
        // 交给调用方拒绝并让用户去站点处理。
        assert_eq!(
            pick_targets(&files, "image_bed", "a.png", false),
            vec!["tg:AAAAAA", "tg:00ff0011"]
        );
        // DAV 显示名带后缀时，后缀（主键末 6 位）能唯一定位到那一条
        assert_eq!(
            pick_targets(&files, "image_bed", "a~ff0011.png", false),
            vec!["tg:00ff0011"]
        );
        // 唯一的名字照旧
        assert_eq!(pick_targets(&files, "other", "a.png", false), vec!["tg:4"]);
        // 目录：把自己和整棵子树都算进去，别目录里的不算
        assert_eq!(
            pick_targets(&files, "", "image_bed", true),
            vec!["tg:AAAAAA", "tg:00ff0011", "tg:3"]
        );
        assert_eq!(pick_targets(&files, "image_bed", "sub", true), vec!["tg:3"]);
        assert!(pick_targets(&files, "image_bed", "nope.png", false).is_empty());
    }

    #[test]
    fn folder_listing_splits_dirs_and_files() {
        let files = vec![
            rf("tg:1", "a.png", "image_bed", 10, "2026-10-23T10:00:00Z"),
            rf("tg:2", "b.png", "image_bed/sub", 20, "2026-10-23T11:00:00Z"),
            rf(
                "tg:3",
                "deep.png",
                "image_bed/sub/deeper",
                30,
                "2026-10-23T12:00:00Z",
            ),
            rf("tg:4", "c.png", "other", 40, "2026-10-23T09:00:00Z"),
        ];
        // 根目录只看到两个目录名：更深层的路径只贡献第一段
        let root = list_folder(&files, "", "name", false);
        let names: Vec<&str> = root.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["image_bed", "other"]);
        assert!(root.iter().all(|i| i.is_dir));
        // 子目录这一层：目录在前，文件在后
        let layer = list_folder(&files, "image_bed", "name", false);
        let names: Vec<&str> = layer.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "a.png"]);
        assert!(layer[0].is_dir);
        assert!(!layer[1].is_dir);
        // 目录的时间取子树里最新的一条，按时间倒序时最近有更新的目录会冒上来
        assert_eq!(layer[0].uploaded, "2026-10-23T12:00:00Z");
        let by_time = list_folder(&files, "image_bed", "mtime", true);
        assert_eq!(by_time[0].name, "sub");
        // 大小排序：目录算 0
        let by_size = list_folder(&files, "image_bed", "size", false);
        assert!(by_size[0].is_dir);
        // 不存在的目录返回空，不 panic
        assert!(list_folder(&files, "nope", "name", false).is_empty());
    }

    #[test]
    fn idem_key_is_stable_and_destination_aware() {
        let a = idem_key("https://h", "telegram", "img", "a.png", 10, 100);
        let b = idem_key("https://h", "telegram", "img", "a.png", 10, 100);
        // 同一份文件传到另一个目录不能被当成同一次上传（否则会拿回旧目录的直链）
        let c = idem_key("https://h", "telegram", "other", "a.png", 10, 100);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("rcgui-"));
        assert_eq!(a.len(), "rcgui-".len() + 16);
    }

    #[test]
    fn id_is_url_encoded_but_keeps_structure() {
        assert_eq!(encode_id("tg:AgACAg"), "tg:AgACAg");
        assert_eq!(encode_id("r2:dir/a b.png"), "r2:dir/a%20b.png");
        assert_eq!(encode_id("tg:a#b"), "tg:a%23b");
    }

}
