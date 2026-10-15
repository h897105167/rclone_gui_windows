//! 图床插件（wasm32-wasip1）：语义移植自旧版 TypeScript 实现。
//! stdin 读入一个 JSON 请求，stdout 写出一个 JSON 响应；需要宿主能力时
//! 先回复 `need`，宿主执行后带 `provided` 重新运行本插件并依据 `state` 续算。

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------- helpers ----------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha1_hex(data: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(data);
    hex(&h.finalize())
}

fn b64decode(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("base64 解码失败: {e}"))
}

/// 拆扩展名：ext 不含点；无扩展名或隐藏文件返回空 ext
fn split_ext(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i + 1..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

/// 规范化子目录：统一正斜杠，去掉空段/./..
fn normalize_sub(dir: &str) -> String {
    dir.replace('\\', "/")
        .split('/')
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .collect::<Vec<_>>()
        .join("/")
}

fn dirname_of(p: &str) -> String {
    match p.rfind('/') {
        Some(i) => p[..i].to_string(),
        None => String::new(),
    }
}

fn basename_of(p: &str) -> String {
    let clean = p.replace('\\', "/");
    match clean.rfind('/') {
        Some(i) => clean[i + 1..].to_string(),
        None => clean,
    }
}

fn join_nonempty(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn s(v: Option<&Value>) -> String {
    v.and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ---------------- config types ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRoot {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub remote: String,
    #[serde(rename = "publicUrlTemplate", default)]
    pub public_url_template: String,
    #[serde(rename = "isDefault", default)]
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Naming {
    #[serde(default = "d_mode")]
    pub mode: String,
    #[serde(rename = "seqPad", default = "d_pad")]
    pub seq_pad: i64,
}
fn d_mode() -> String { "timestamp".into() }
fn d_pad() -> i64 { 3 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObsidianCfg {
    #[serde(rename = "dirLayout", default = "d_layout")]
    pub dir_layout: String,
}
fn d_layout() -> String { "note".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub token: String,
    #[serde(rename = "createdAt", default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImagehostConfig {
    #[serde(default = "d_roots")]
    pub roots: Vec<ImageRoot>,
    #[serde(default = "d_naming")]
    pub naming: Naming,
    #[serde(default = "d_true")]
    pub dedupe: bool,
    #[serde(default)]
    pub trash_dir: String,
    #[serde(default = "d_obs")]
    pub obsidian: ObsidianCfg,
    #[serde(default)]
    pub tokens: Vec<Token>,
}
fn d_true() -> bool { true }
fn d_roots() -> Vec<ImageRoot> {
    vec![ImageRoot {
        id: "default".into(),
        name: "默认图床".into(),
        remote: "teldrive:图床".into(),
        public_url_template: String::new(),
        is_default: true,
    }]
}
fn d_naming() -> Naming { Naming { mode: d_mode(), seq_pad: d_pad() } }
fn d_obs() -> ObsidianCfg { ObsidianCfg { dir_layout: d_layout() } }

impl ImagehostConfig {
    fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
    /// 保证有默认 root 标记
    fn normalize(&mut self) {
        if !self.roots.iter().any(|r| r.is_default) {
            for (i, r) in self.roots.iter_mut().enumerate() {
                r.is_default = i == 0;
            }
        }
    }
}

fn load_config(kv: &Value) -> ImagehostConfig {
    let raw = kv.get("config").cloned().unwrap_or(Value::Null);
    let mut cfg: ImagehostConfig = serde_json::from_value(raw).unwrap_or(ImagehostConfig {
        roots: d_roots(),
        naming: d_naming(),
        dedupe: true,
        trash_dir: String::new(),
        obsidian: d_obs(),
        tokens: vec![],
    });
    if cfg.roots.is_empty() {
        cfg.roots = d_roots();
    }
    cfg.normalize();
    cfg
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageItem {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "rootId", default)]
    pub root_id: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub sha1: String,
    #[serde(default)]
    pub ext: String,
    #[serde(rename = "uploadedAt", default)]
    pub uploaded_at: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn load_index(kv: &Value) -> Vec<ImageItem> {
    let raw = kv.get("index").cloned().unwrap_or(Value::Null);
    let items = raw.get("items").cloned().unwrap_or(Value::Array(vec![]));
    serde_json::from_value(items).unwrap_or_default()
}

// ---------------- naming（移植 naming.ts） ----------------

/// timestamp: YYYYMMDDHHmmssSSS（14 位 + 毫秒 3 位防冲突，来自宿主本地时间）
fn timestamp_name(now: &Value) -> String {
    format!("{}{}{}", s(now.get("ymd")), s(now.get("hms")), s(now.get("ms")))
}

/// seq: 已有文件名中纯数字序号最大值 +1，补零
fn next_seq_name(names: &[String], pad: i64) -> String {
    let mut max = 0i64;
    for n in names {
        let base = split_ext(n).0;
        if !base.is_empty() && base.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(v) = base.parse::<i64>() {
                max = max.max(v);
            }
        }
    }
    format!("{:0width$}", max + 1, width = pad.max(1) as usize)
}

/// 冲突：同名且不同 sha1 时追加 -1/-2
fn resolve_conflict_name(existing: &std::collections::HashSet<String>, base: &str, ext: &str) -> String {
    let suffix = if ext.is_empty() { String::new() } else { format!(".{ext}") };
    if !existing.contains(&format!("{base}{suffix}")) {
        return format!("{base}{suffix}");
    }
    for n in 1..1000 {
        let cand = format!("{base}-{n}{suffix}");
        if !existing.contains(&cand) {
            return cand;
        }
    }
    format!("{base}-{}{suffix}", epoch_ms())
}

/// dirLayout=note: obsidian/<vault>/<dirname(notePath)>/
/// dirLayout=date:  obsidian/<vault>/<yyyy-MM-dd>/
/// dirLayout=vault: obsidian/<vault>/
/// dirLayout=flat:  obsidian/
fn obsidian_sub_path(layout: &str, vault: &str, note_path: &str, today: &str) -> String {
    let v = normalize_sub(vault);
    let note_dir = dirname_of(&normalize_sub(note_path));
    match layout {
        "date" => join_nonempty(&["obsidian", &v, today]),
        "vault" => join_nonempty(&["obsidian", &v]),
        "flat" => "obsidian".to_string(),
        _ => join_nonempty(&["obsidian", &v, &note_dir]),
    }
}

// ---------------- index 查询（移植 store.ts） ----------------

fn find_by_path<'a>(items: &'a [ImageItem], root_id: &str, p: &str) -> Option<&'a ImageItem> {
    items.iter().find(|i| i.root_id == root_id && i.path == p)
}

fn find_by_sha<'a>(items: &'a [ImageItem], root_id: &str, sha1: &str) -> Option<&'a ImageItem> {
    if sha1.is_empty() {
        return None;
    }
    items.iter().find(|i| i.root_id == root_id && i.sha1 == sha1)
}

/// 索引中某目录直属文件名（用于 seq 计数）
fn names_in_dir(items: &[ImageItem], root_id: &str, dir: &str) -> Vec<String> {
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    items
        .iter()
        .filter(|i| i.root_id == root_id && i.path.starts_with(&prefix) && !i.path[prefix.len()..].contains('/'))
        .map(|i| i.path[prefix.len()..].to_string())
        .collect()
}

fn item_id(root_id: &str, path: &str) -> String {
    let h = sha1_hex(format!("{root_id}:{path}").as_bytes());
    h[..16].to_string()
}

// ---------------- 请求结构 ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MultipartFile {
    #[serde(default)]
    filename: String,
    #[serde(rename = "dataB64", default)]
    data_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Multipart {
    #[serde(default)]
    fields: Map<String, Value>,
    #[serde(default)]
    files: Vec<MultipartFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HttpReq {
    #[serde(default)]
    method: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    query: Map<String, Value>,
    #[serde(default)]
    authorization: String,
    #[serde(rename = "bodyB64", default)]
    body_b64: String,
    #[serde(default)]
    multipart: Option<Multipart>,
    #[serde(default)]
    origin: String,
}

#[derive(Debug, Deserialize)]
struct State {
    #[serde(default)]
    http: Option<HttpReq>,
    #[serde(default)]
    now: Value,
    #[serde(rename = "reqId", default)]
    req_id: String,
    // 上传/扫描续算数据
    #[serde(default)]
    cfg: Value,
    #[serde(default)]
    index: Vec<ImageItem>,
    #[serde(default)]
    root: Option<ImageRoot>,
    #[serde(default)]
    dir: String,
    #[serde(default)]
    files: Vec<MultipartFile>,
    #[serde(rename = "sha1s", default)]
    sha1s: Vec<String>,
    #[serde(rename = "sizes", default)]
    sizes: Vec<i64>,
}

fn base_state(http: &Option<HttpReq>, now: &Value, req_id: &str) -> Value {
    json!({ "http": http, "now": now, "reqId": req_id })
}

// ---------------- 入口 ----------------

fn main() {
    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        return;
    }
    let resp = handle(&input).unwrap_or_else(|e| json!({"id": 1, "error": {"message": e}}));
    let mut out = std::io::stdout();
    let _ = out.write_all(&serde_json::to_vec(&resp).unwrap_or_default());
    let _ = out.flush();
}

fn handle(input: &[u8]) -> Result<Value, String> {
    let req: Value = serde_json::from_slice(input).map_err(|e| format!("请求解析失败: {e}"))?;
    let id = req.get("id").and_then(|v| v.as_u64()).unwrap_or(1);
    let method = s(req.get("method"));
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    match method.as_str() {
        "manifest" => Ok(json!({
            "id": id,
            "manifest": {
                "id": "imagehost",
                "name": "图床",
                "version": "0.2.0",
                "description": "基于 rclone 的图床：上传、去重、索引、直链代理",
            }
        })),
        "request" => handle_request(id, params),
        other => Err(format!("未知 method: {other}")),
    }
}

fn ok(id: u64, body: Value) -> Value {
    json!({"id": id, "ops": [], "result": {"status": 200, "body": body}})
}

fn ok_with_ops(id: u64, ops: Vec<Value>, body: Value) -> Value {
    json!({"id": id, "ops": ops, "result": {"status": 200, "body": body}})
}

fn err(id: u64, status: u16, msg: &str) -> Value {
    json!({"id": id, "ops": [], "result": {"status": status, "body": {"error": msg}}})
}

/// 第一轮：请求 kv；第二轮起：路由分发（第三轮带 provided.entries）
fn handle_request(id: u64, params: Value) -> Result<Value, String> {
    let provided = params.get("provided").cloned().unwrap_or(Value::Null);
    if provided.is_null() {
        let http: Option<HttpReq> = match params.get("http") {
            Some(v) if !v.is_null() => Some(
                serde_json::from_value(v.clone()).map_err(|e| format!("http 参数解析失败: {e}"))?,
            ),
            _ => None,
        };
        let state = base_state(
            &http,
            &params.get("now").cloned().unwrap_or(Value::Null),
            &s(params.get("req_id")),
        );
        return Ok(json!({
            "id": id,
            "need": [{"cap": "kv.get_all", "key": "kv"}],
            "state": state,
        }));
    }

    let kv = provided.get("kv").cloned().unwrap_or(json!({}));
    let entries: Option<Vec<Value>> = match provided.get("entries") {
        Some(v) => {
            // 宿主 rclone.call 返回原始响应（{"list":[...]}），数组也兼容
            let raw = if v.is_array() {
                v.clone()
            } else {
                v.get("list").cloned().unwrap_or(Value::Null)
            };
            Some(serde_json::from_value(raw).map_err(|e| format!("entries 解析失败: {e}"))?)
        }
        None => None,
    };
    let state: State = serde_json::from_value(params.get("state").cloned().unwrap_or(Value::Null))
        .map_err(|e| format!("state 解析失败: {e}"))?;
    let http = state.http.clone().ok_or("缺少 http 参数")?;
    let now = state.now.clone();
    let req_id = state.req_id.clone();
    // 第三轮 kv 不在 provided 中，优先用 state 快照（第二轮保存）
    let mut cfg = match serde_json::from_value::<ImagehostConfig>(state.cfg.clone()) {
        Ok(c) => c,
        Err(_) => load_config(&kv),
    };
    let mut index = if state.index.is_empty() {
        load_index(&kv)
    } else {
        state.index.clone()
    };

    let path = http.path.trim().trim_end_matches('/').to_string();
    let path = if path.is_empty() { "/".to_string() } else { path };
    let method = http.method.to_uppercase();

    // token 校验（配置了 token 时，仅对上传/扫描生效）
    let needs_auth = !cfg.tokens.is_empty() && matches!((method.as_str(), path.as_str()), ("POST", "/upload") | ("POST", "/scan"));
    if needs_auth && !check_token(&http.authorization, &cfg) {
        return Ok(err(id, 401, "unauthorized"));
    }

    match (method.as_str(), path.as_str()) {
        ("GET", "/config") => Ok(ok(id, cfg.to_value())),
        ("PUT", "/config") => put_config(id, &http, cfg),
        ("POST", "/token") => post_token(id, &http, &mut cfg, &req_id, &now),
        ("DELETE", p) if p.starts_with("/token/") => delete_token(id, &p["/token/".len()..], cfg),
        ("GET", "/files") => Ok(ok(id, query_files(&index, &http.query))),
        ("GET", "/stats") => Ok(ok(id, stats(&index))),
        ("POST", "/files/delete") => files_delete(id, &http, &cfg, &mut index, &now),
        ("POST", "/files/rename") => files_rename(id, &http, &cfg, &mut index),
        ("POST", "/scan") => scan_phase(id, &http, cfg, index, state, entries),
        ("POST", "/upload") => upload_phase(id, &http, cfg, index, state, entries),
        _ => Ok(err(id, 404, "路由不存在")),
    }
}

fn check_token(authorization: &str, cfg: &ImagehostConfig) -> bool {
    let t = authorization
        .strip_prefix("Bearer ")
        .or_else(|| authorization.strip_prefix("bearer "))
        .map(|x| x.trim().to_string())
        .unwrap_or_default();
    !t.is_empty() && cfg.tokens.iter().any(|x| x.token == t)
}

// ---------------- /config ----------------

fn put_config(id: u64, http: &HttpReq, old: ImagehostConfig) -> Result<Value, String> {
    let body = b64decode(&http.body_b64)?;
    let v: Value = serde_json::from_slice(&body).map_err(|e| format!("请求体解析失败: {e}"))?;
    let roots_empty = v.get("roots").and_then(|r| r.as_array()).map(|a| a.is_empty()).unwrap_or(true);
    if roots_empty {
        return Ok(err(id, 400, "至少需要一个 root"));
    }
    let roots: Vec<ImageRoot> = match serde_json::from_value(v.get("roots").cloned().unwrap_or(Value::Null)) {
        Ok(r) => r,
        Err(_) => return Ok(err(id, 400, "root 需要 id 与 remote")),
    };
    for r in &roots {
        if r.id.is_empty() || r.remote.is_empty() {
            return Ok(err(id, 400, "root 需要 id 与 remote"));
        }
    }
    let naming = v.get("naming").cloned().unwrap_or(Value::Null);
    let mode = {
        let m = s(naming.get("mode"));
        if ["timestamp", "seq", "keep", "hash"].contains(&m.as_str()) { m } else { old.naming.mode.clone() }
    };
    let cfg = ImagehostConfig {
        roots,
        naming: Naming {
            mode,
            seq_pad: naming.get("seqPad").and_then(|x| x.as_i64()).unwrap_or(old.naming.seq_pad),
        },
        dedupe: v.get("dedupe").and_then(|x| x.as_bool()).unwrap_or(old.dedupe),
        trash_dir: if v.get("trashDir").is_some() { s(v.get("trashDir")) } else { old.trash_dir.clone() },
        obsidian: ObsidianCfg {
            dir_layout: {
                let l = s(v.get("obsidian").and_then(|o| o.get("dirLayout")));
                if l.is_empty() { old.obsidian.dir_layout.clone() } else { l }
            },
        },
        tokens: match v.get("tokens") {
            Some(Value::Array(_)) => {
                serde_json::from_value(v.get("tokens").cloned().unwrap_or(Value::Null)).unwrap_or(old.tokens.clone())
            }
            _ => old.tokens.clone(),
        },
    };
    let mut cfg = cfg;
    cfg.normalize();
    Ok(ok_with_ops(id, vec![json!({"op": "kv.set", "key": "config", "value": cfg.to_value()})], cfg.to_value()))
}

// ---------------- /token ----------------

fn post_token(id: u64, http: &HttpReq, cfg: &mut ImagehostConfig, req_id: &str, now: &Value) -> Result<Value, String> {
    let body = b64decode(&http.body_b64)?;
    let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let name = {
        let n = s(v.get("name"));
        if n.trim().is_empty() { "未命名".to_string() } else { n }
    };
    // 随机性来自宿主 req_id（uuid hex）+ 时间戳
    let mut seed = format!("{req_id}{}", s(now.get("epoch_ms")));
    seed.truncate(24);
    let token = Token {
        id: req_id.to_string(),
        name,
        token: seed,
        created_at: s(now.get("iso")),
    };
    cfg.tokens.push(token.clone());
    Ok(ok_with_ops(
        id,
        vec![json!({"op": "kv.set", "key": "config", "value": cfg.to_value()})],
        serde_json::to_value(&token).unwrap_or(Value::Null),
    ))
}

fn delete_token(id: u64, tid: &str, mut cfg: ImagehostConfig) -> Result<Value, String> {
    let before = cfg.tokens.len();
    cfg.tokens.retain(|t| t.id != tid);
    if cfg.tokens.len() == before {
        return Ok(err(id, 404, "token 不存在"));
    }
    Ok(ok_with_ops(
        id,
        vec![json!({"op": "kv.set", "key": "config", "value": cfg.to_value()})],
        json!({"ok": true}),
    ))
}

// ---------------- /files 查询与统计 ----------------

fn query_files(index: &[ImageItem], query: &Map<String, Value>) -> Value {
    let get_q = |k: &str| s(query.get(k));
    let root_id = get_q("rootId");
    let q = get_q("q").to_lowercase();
    let page = get_q("page").parse::<usize>().unwrap_or(1).max(1);
    let page_size = get_q("pageSize").parse::<usize>().unwrap_or(60).clamp(1, 200);

    let mut list: Vec<&ImageItem> = index
        .iter()
        .filter(|i| root_id.is_empty() || i.root_id == root_id)
        .filter(|i| q.is_empty() || i.name.to_lowercase().contains(&q) || i.path.to_lowercase().contains(&q))
        .collect();
    list.sort_by(|a, b| b.uploaded_at.cmp(&a.uploaded_at));
    let total = list.len();
    let items: Vec<&ImageItem> = list
        .into_iter()
        .skip((page - 1) * page_size)
        .take(page_size)
        .collect();
    json!({
        "items": items,
        "total": total,
        "page": page,
        "pageSize": page_size,
    })
}

fn stats(index: &[ImageItem]) -> Value {
    let mut map: std::collections::BTreeMap<String, (i64, i64)> = std::collections::BTreeMap::new();
    let mut count = 0i64;
    let mut bytes = 0i64;
    for i in index {
        let e = map.entry(i.root_id.clone()).or_insert((0, 0));
        e.0 += 1;
        e.1 += i.size;
        count += 1;
        bytes += i.size;
    }
    let roots: Vec<Value> = map
        .into_iter()
        .map(|(root_id, (c, b))| json!({"rootId": root_id, "count": c, "bytes": b}))
        .collect();
    json!({"count": count, "bytes": bytes, "roots": roots})
}

// ---------------- root 选择 ----------------

fn pick_root<'a>(cfg: &'a ImagehostConfig, root_id: &str) -> Option<&'a ImageRoot> {
    if !root_id.is_empty() {
        return cfg.roots.iter().find(|r| r.id == root_id);
    }
    cfg.roots.iter().find(|r| r.is_default).or_else(|| cfg.roots.first())
}

// ---------------- /files/delete、/files/rename ----------------

fn files_delete(id: u64, http: &HttpReq, cfg: &ImagehostConfig, index: &mut Vec<ImageItem>, now: &Value) -> Result<Value, String> {
    let body = b64decode(&http.body_b64)?;
    let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let root_id = s(v.get("rootId"));
    let root = match pick_root(cfg, &root_id) {
        Some(r) => r,
        None => return Ok(err(id, 400, "root 不存在")),
    };
    let paths: Vec<String> = v
        .get("paths")
        .and_then(|p| p.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|y| y.to_string())).collect())
        .unwrap_or_default();
    if paths.is_empty() {
        return Ok(err(id, 400, "paths 不能为空"));
    }
    let to_trash = v.get("toTrash").and_then(|x| x.as_bool()).unwrap_or(false) && !cfg.trash_dir.is_empty();
    let today = s(now.get("date_iso"));
    let mut deleted = Vec::new();
    let mut ops = Vec::new();
    for p in &paths {
        let clean = normalize_sub(p);
        if clean.is_empty() {
            continue;
        }
        if to_trash {
            let name = basename_of(&clean);
            let dst = format!("{}/{}/{}", normalize_sub(&cfg.trash_dir), today, name);
            ops.push(json!({
                "op": "rclone.call",
                "method": "operations/movefile",
                "params": {"srcFs": root.remote, "srcRemote": clean, "dstFs": root.remote, "dstRemote": dst},
            }));
        } else {
            ops.push(json!({
                "op": "rclone.call",
                "method": "operations/deletefile",
                "params": {"fs": root.remote, "remote": clean},
            }));
        }
        deleted.push(clean);
    }
    index.retain(|i| !(i.root_id == root.id && deleted.contains(&i.path)));
    ops.push(json!({"op": "kv.set", "key": "index", "value": {"items": index}}));
    Ok(ok_with_ops(id, ops, json!({"deleted": deleted})))
}

fn files_rename(id: u64, http: &HttpReq, cfg: &ImagehostConfig, index: &mut Vec<ImageItem>) -> Result<Value, String> {
    let body = b64decode(&http.body_b64)?;
    let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let root_id = s(v.get("rootId"));
    let root = match pick_root(cfg, &root_id) {
        Some(r) => r,
        None => return Ok(err(id, 400, "root 不存在")),
    };
    let from = normalize_sub(&s(v.get("from")));
    let to = s(v.get("to"));
    if from.is_empty() || to.is_empty() {
        return Ok(err(id, 400, "from/to 必填"));
    }
    if to.contains('/') || to.contains('\\') || to == "." || to == ".." {
        return Ok(err(id, 400, "to 只能是文件名"));
    }
    let dir = dirname_of(&from);
    let to_path = if dir.is_empty() { to.clone() } else { format!("{dir}/{to}") };
    let ops = vec![
        json!({
            "op": "rclone.call",
            "method": "operations/movefile",
            "params": {"srcFs": root.remote, "srcRemote": from, "dstFs": root.remote, "dstRemote": to_path},
        }),
        json!({"op": "kv.set", "key": "index", "value": {"items": index}}),
    ];
    if let Some(it) = index.iter_mut().find(|i| i.root_id == root.id && i.path == from) {
        it.path = to_path.clone();
        it.name = to;
    }
    Ok(ok_with_ops(id, ops, json!({"path": to_path})))
}

// ---------------- /scan：三轮协商 ----------------

fn scan_phase(id: u64, http: &HttpReq, cfg: ImagehostConfig, index: Vec<ImageItem>, state: State, entries: Option<Vec<Value>>) -> Result<Value, String> {
    let root = match &state.root {
        Some(r) => r.clone(),
        None => {
            // 第二轮：解析 root，请求递归列表
            let body = b64decode(&http.body_b64)?;
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let root_id = s(v.get("rootId"));
            match pick_root(&cfg, &root_id) {
                Some(r) => r.clone(),
                None => return Ok(err(id, 400, "root 不存在")),
            }
        }
    };

    if entries.is_none() {
        let mut st = base_state(&state.http, &state.now, &state.req_id);
        st["cfg"] = cfg.to_value();
        st["index"] = serde_json::to_value(&index).unwrap_or(Value::Null);
        st["root"] = serde_json::to_value(&root).unwrap_or(Value::Null);
        return Ok(json!({
            "id": id,
            "need": [{
                "cap": "rclone.call",
                "key": "entries",
                "params": {"method": "operations/list", "params": {"fs": root.remote, "remote": "", "options": {"recurse": true}}},
            }],
            "state": st,
        }));
    }

    // 第三轮：回填索引
    let mut index = index;
    let entries = entries.unwrap_or_default();
    let now = state.now.clone();
    let mut added = 0i64;
    for e in entries.iter().take(5000) {
        let is_dir = e.get("IsDir").and_then(|x| x.as_bool()).unwrap_or(false);
        let path = normalize_sub(&s(e.get("Path")));
        if is_dir || path.is_empty() {
            continue;
        }
        if find_by_path(&index, &root.id, &path).is_some() {
            continue;
        }
        let name = s(e.get("Name"));
        let (_, ext) = split_ext(&name);
        index.push(ImageItem {
            id: item_id(&root.id, &path),
            root_id: root.id.clone(),
            path: path.clone(),
            name,
            size: e.get("Size").and_then(|x| x.as_i64()).unwrap_or(0),
            sha1: String::new(),
            ext: ext.to_lowercase(),
            uploaded_at: {
                let m = s(e.get("ModTime"));
                if m.is_empty() { s(now.get("iso")) } else { m }
            },
            source: "scan".into(),
            tags: vec![],
        });
        added += 1;
    }
    let total = index.iter().filter(|i| i.root_id == root.id).count();
    let ops = vec![json!({"op": "kv.set", "key": "index", "value": {"items": index}})];
    Ok(ok_with_ops(id, ops, json!({"added": added, "total": total})))
}

// ---------------- /upload：三轮协商（移植 pipeline.ts） ----------------

fn upload_phase(
    id: u64,
    http: &HttpReq,
    cfg: ImagehostConfig,
    index: Vec<ImageItem>,
    state: State,
    entries: Option<Vec<Value>>,
) -> Result<Value, String> {
    // 文件来源：multipart（网页/应用上传）或 raw body（Obsidian 等）
    let (fields, files): (Map<String, Value>, Vec<MultipartFile>) = if let Some(mp) = &http.multipart {
        (mp.fields.clone(), mp.files.clone())
    } else {
        let filename = s(http.query.get("filename"));
        if filename.is_empty() {
            return Ok(err(id, 400, "raw 模式缺少 filename 参数"));
        }
        if http.body_b64.is_empty() {
            return Ok(err(id, 400, "请求体为空"));
        }
        let mut f = Map::new();
        for k in ["rootId", "subPath", "naming", "seqPad", "app", "vault", "notePath"] {
            if let Some(v) = http.query.get(k) {
                f.insert(k.to_string(), v.clone());
            }
        }
        (f, vec![MultipartFile { filename: basename_of(&filename), data_b64: http.body_b64.clone() }])
    };
    if files.is_empty() {
        return Ok(err(id, 400, "缺少文件"));
    }

    let getf = |k: &str| s(fields.get(k));
    let root_id = getf("rootId");
    let root = match pick_root(&cfg, &root_id) {
        Some(r) => r.clone(),
        None => return Ok(err(id, 400, "root 不存在")),
    };

    let now = state.now.clone();

    if state.root.is_none() {
        // 第二轮：算 sha1 / 目标目录，请求目标目录列表
        let dir = if getf("app") == "obsidian" {
            obsidian_sub_path(&cfg.obsidian.dir_layout, &getf("vault"), &getf("notePath"), &s(now.get("date_iso")))
        } else {
            normalize_sub(&getf("subPath"))
        };
        let mut sha1s = Vec::new();
        let mut sizes = Vec::new();
        for f in &files {
            match b64decode(&f.data_b64) {
                Ok(d) => {
                    sizes.push(d.len() as i64);
                    sha1s.push(sha1_hex(&d));
                }
                Err(_) => {
                    sha1s.push(String::new());
                    sizes.push(0);
                }
            }
        }
        let mut st = base_state(&state.http, &state.now, &state.req_id);
        st["cfg"] = cfg.to_value();
        st["index"] = serde_json::to_value(&index).unwrap_or(Value::Null);
        st["root"] = serde_json::to_value(&root).unwrap_or(Value::Null);
        st["dir"] = Value::String(dir.clone());
        st["files"] = serde_json::to_value(&files).unwrap_or(Value::Null);
        st["sha1s"] = serde_json::to_value(&sha1s).unwrap_or(Value::Null);
        st["sizes"] = serde_json::to_value(&sizes).unwrap_or(Value::Null);
        return Ok(json!({
            "id": id,
            "need": [{
                "cap": "rclone.call",
                "key": "entries",
                "params": {"method": "operations/list", "params": {"fs": root.remote, "remote": dir}},
            }],
            "state": st,
        }));
    }

    // 第三轮：命名 → 冲突 → ops（上传 + 索引）→ 结果
    let mut index = index;
    let dir_entries = entries.unwrap_or_default();
    let dir = state.dir.clone();
    let files = state.files;
    let sha1s = state.sha1s;
    let sizes = state.sizes;

    let raw_mode = {
        let m = getf("naming");
        if ["timestamp", "seq", "keep", "hash"].contains(&m.as_str()) { m } else { cfg.naming.mode.clone() }
    };
    let seq_pad = {
        let p = getf("seqPad").parse::<i64>().unwrap_or(0);
        if p > 0 { p } else { cfg.naming.seq_pad }
    };

    // 目标目录现状（文件名集合）
    let mut existing: std::collections::HashSet<String> = dir_entries
        .iter()
        .filter(|e| !e.get("IsDir").and_then(|x| x.as_bool()).unwrap_or(false))
        .filter_map(|e| e.get("Name").and_then(|x| x.as_str()).map(|x| x.to_string()))
        .collect();
    for n in names_in_dir(&index, &root.id, &dir) {
        existing.insert(n);
    }

    let mut results: Vec<Value> = Vec::new();
    let mut ops: Vec<Value> = Vec::new();
    let mut new_items: Vec<ImageItem> = Vec::new();

    for (i, f) in files.iter().enumerate() {
        let orig_name = {
            let n = basename_of(&f.filename);
            if n.is_empty() { "file".to_string() } else { n }
        };
        let sha1 = sha1s.get(i).cloned().unwrap_or_default();
        let data_b64 = f.data_b64.clone();
        let size = sizes.get(i).copied().unwrap_or(0);

        // 1) 去重秒传
        if cfg.dedupe {
            if let Some(exist) = find_by_sha(&index, &root.id, &sha1) {
                results.push(to_result(exist, &root, &http.origin, true));
                continue;
            }
        }

        // 4) 命名
        let (orig_base, orig_ext) = split_ext(&orig_name);
        let ext = orig_ext.to_lowercase();
        let base = match raw_mode.as_str() {
            "keep" => orig_base,
            "hash" => sha1.chars().take(12).collect::<String>(),
            "seq" => {
                let names: Vec<String> = existing.iter().cloned().collect();
                next_seq_name(&names, seq_pad)
            }
            _ => timestamp_name(&now),
        };

        // 5) 冲突
        let mut final_name = if ext.is_empty() { base.clone() } else { format!("{base}.{ext}") };
        if existing.contains(&final_name) {
            let stored_candidate = if dir.is_empty() { final_name.clone() } else { format!("{dir}/{final_name}") };
            if let Some(conflict) = find_by_path(&index, &root.id, &stored_candidate) {
                if conflict.sha1 == sha1 {
                    results.push(to_result(conflict, &root, &http.origin, true));
                    continue;
                }
            }
            final_name = resolve_conflict_name(&existing, &base, &ext);
        }
        existing.insert(final_name.clone());

        let stored_path = if dir.is_empty() { final_name.clone() } else { format!("{dir}/{final_name}") };

        // 6) 上传 op
        ops.push(json!({
            "op": "rclone.upload_file",
            "fs": root.remote,
            "remote_path": stored_path,
            "filename": final_name,
            "data_b64": data_b64,
        }));

        // 7) 索引
        let item = ImageItem {
            id: item_id(&root.id, &stored_path),
            root_id: root.id.clone(),
            path: stored_path,
            name: final_name,
            size,
            sha1,
            ext,
            uploaded_at: s(now.get("iso")),
            source: "upload".into(),
            tags: vec![],
        };
        results.push(to_result(&item, &root, &http.origin, false));
        new_items.push(item);
    }

    for it in new_items {
        if let Some(pos) = index.iter().position(|x| x.root_id == it.root_id && x.path == it.path) {
            index[pos] = it;
        } else {
            index.push(it);
        }
    }
    ops.push(json!({"op": "kv.set", "key": "index", "value": {"items": index}}));
    Ok(ok_with_ops(id, ops, json!({"items": results})))
}

/// 模板渲染：{path} 按段 encodeURIComponent 后以 / 连接；{filename} 编码文件名
fn render_public_url(template: &str, stored_path: &str, name: &str) -> String {
    let p = stored_path
        .split('/')
        .map(encodeURIComponent)
        .collect::<Vec<_>>()
        .join("/");
    template
        .replace("{path}", &p)
        .replace("{filename}", &encodeURIComponent(name))
}

fn encodeURIComponent(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'!' | b'*' | b'\'' | b'(' | b')' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn to_result(item: &ImageItem, root: &ImageRoot, origin: &str, deduped: bool) -> Value {
    let url = format!("{origin}/i/{}/{}", root.id, item.path);
    let public_url = if root.public_url_template.is_empty() {
        Value::Null
    } else {
        Value::String(render_public_url(&root.public_url_template, &item.path, &item.name))
    };
    json!({
        "name": item.name,
        "storedPath": item.path,
        "url": url,
        "publicUrl": public_url,
        "size": item.size,
        "sha1": item.sha1,
        "deduped": deduped,
        "markdown": format!("![{}]({})", item.name, url),
    })
}
