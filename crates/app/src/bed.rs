//! 图床面板：把「选一张本机图片 → 传到远程 → 拿直链 → 换短链」串成一步。
//!
//! 为什么单独一个模块、而不是塞进 imagehost（WASM 插件）：
//! - 直链识别需要两样插件拿不到的东西：`rclone.exe`（`rclone link` / `rclone reveal`）
//!   与 rclone.conf 里的远程参数（url / user / pass）；
//! - 短链要发外部 HTTP 请求，且要按「每秒 N 次」排队，插件侧没有跨调用状态。
//! 所以「存储根 / 命名 / 令牌 / 文件列表」仍归插件，「上传 → 直链 → 短链」做在核心 app。
//!
//! 直链识别顺序（用户要求「按远程类型自动识别」）：
//! 1. `rclone link <remote>:<path>` —— 支持公开链接的后端走这条（teldrive / S3 系）；
//! 2. AList 管理 API —— `type = webdav` 且站点探活为 AList 时，
//!    用 conf 里的 username/password 换管理员 token，再 `/api/fs/get` 拿 `raw_url`；
//! 3. 本机代理 `/i/<root>/<path>` —— 前两条都失败时的兜底（只有本机能访问）。
//!
//! 注意（实测）：rclone 的 webdav 后端**不支持** `link`，对 AList 远程固定报
//! `Failed to link: webdav root 'xxx' doesn't support public links`，
//! 所以第 1 步对 AList 必然失败、必须靠第 2 步。这里保留第 1 步是因为它
//! 对其他后端（teldrive / S3）是唯一正确路径，而且失败很快（不会拖慢 UI）。

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use rclone_client::{discover_exe, RcClient};
use std::time::{Duration, Instant};

/// 配置落在 imagehost 插件的 kv 命名空间下：与 roots / tokens 同一个文件，方便一起备份迁移。
pub const NS: &str = "imagehost";
pub const KEY: &str = "bed";

/// 短链模板，顺序与 UI 下拉框一一对应（0 = 不选择 = 不生成短链）。
pub const TEMPLATES: [&str; 2] = ["不选择", "模板一"];
/// 短链响应格式，顺序与 UI 下拉框一一对应。
pub const FORMATS: [&str; 3] = ["无", "JSON", "JSONP"];
/// 请求方式，顺序与 UI 下拉框一一对应。
pub const METHODS: [&str; 2] = ["GET", "POST"];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BedCfg {
    /// 存储的远程（remote 名，不带冒号）
    #[serde(default)]
    pub remote: String,
    /// 远端路径（`/img` 或 `img`，空 = 远程根）
    #[serde(default)]
    pub path: String,
    /// 上传图片路径：本机文件（选一次就记住）
    #[serde(default)]
    pub upload_path: String,
    /// 短链模板选择（0 不选择 / 1 模板一）：不选择时只出直链
    #[serde(default)]
    pub short_tpl: usize,
    /// 短链模板一：输出格式下标（0 无 / 1 JSON / 2 JSONP）
    #[serde(default)]
    pub short_fmt: usize,
    /// 短链模板一：回调函数名（仅 JSONP 用）
    #[serde(default)]
    pub short_callback: String,
    /// 短链模板一：请求地址
    #[serde(default)]
    pub short_url: String,
    /// 短链模板一：请求方式下标（0 GET / 1 POST）
    #[serde(default)]
    pub short_method: usize,
    /// 短链模板一：请求频率（次/秒，默认 4）
    #[serde(default = "d_rate")]
    pub short_rate: u32,
    /// 短链模板一：api_key
    #[serde(default)]
    pub short_key: String,
    /// 上一次生成的直链（面板上直接显示，重启后也能复制）
    #[serde(default)]
    pub last_link: String,
    /// 上一次生成的短链
    #[serde(default)]
    pub last_short: String,
    /// 去重：开启时目标路径已有同名文件就提示并取消上传；关闭时改成 `名字 (1).扩展名`
    #[serde(default = "d_true")]
    pub dedupe: bool,
    /// 用过的远端路径，最新在前，最多 [`HISTORY_MAX`] 条（面板下拉框直接铺这个）
    #[serde(default)]
    pub path_history: Vec<String>,
}

/// 路径历史最多记多少条。
pub const HISTORY_MAX: usize = 10;

fn d_true() -> bool {
    true
}

fn d_rate() -> u32 {
    4
}

impl Default for BedCfg {
    fn default() -> Self {
        Self {
            remote: String::new(),
            path: String::new(),
            upload_path: String::new(),
            short_tpl: 0,
            short_fmt: 0,
            short_callback: "callback".into(),
            short_url: String::new(),
            short_method: 0,
            short_rate: d_rate(),
            short_key: String::new(),
            last_link: String::new(),
            last_short: String::new(),
            dedupe: true,
            path_history: Vec::new(),
        }
    }
}

impl BedCfg {
    pub fn fmt_name(&self) -> &'static str {
        FORMATS.get(self.short_fmt).copied().unwrap_or("无")
    }

    pub fn method_name(&self) -> &'static str {
        METHODS.get(self.short_method).copied().unwrap_or("GET")
    }

    /// 记一条用过的路径：去重、挪到最前、只留前 [`HISTORY_MAX`] 条。
    ///
    /// 空路径（= 远程根）也记：用户经常就在根目录传图，不记的话下拉框永远是空的。
    /// 记住用过的远端路径（最新的在最前，最多 [`HISTORY_MAX`] 条）。
    ///
    /// 远程根（空路径）存成 `/`：下拉框里显示成空行会让人以为没记住，
    /// 而 `/` 回填进「路径」后跟留空是同一个意思（两侧斜杠都会被去掉）。
    pub fn remember_path(&mut self, path: &str) {
        let trimmed = path.trim().trim_matches('/');
        let p = if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() };
        self.path_history.retain(|x| x != &p);
        self.path_history.insert(0, p);
        self.path_history.truncate(HISTORY_MAX);
    }
}

// ---------------------------------------------------------------- 纯函数部分

/// 远端存储路径：`("img", "a.png")` → `img/a.png`；`("", "a.png")` → `a.png`。
///
/// 统一去掉首尾 `/`：rclone 的 fs 串写成 `remote:img`（`remote:/img` 也能用，
/// 但拼 `/i/<root>/<path>` 代理链接时多一个斜杠会变成 `//`，容易被反代改写）。
pub fn join_stored(dir: &str, name: &str) -> String {
    let d = dir.trim().trim_matches('/');
    let n = name.trim().trim_start_matches('/');
    match (d.is_empty(), n.is_empty()) {
        (true, _) => n.to_string(),
        (false, true) => d.to_string(),
        (false, false) => format!("{d}/{n}"),
    }
}

/// rclone 的「远程:路径」串，用于 `rclone link`。
pub fn mount_path(remote: &str, stored: &str) -> String {
    let r = remote.trim().trim_end_matches(':');
    format!("{r}:{stored}")
}

/// AList 站点根的候选地址：webdav 远程的 url 常写成 `https://host/dav`，
/// 而管理 API 在站点根（`https://host/api/...`），两个都要试。
pub fn alist_base_candidates(raw: &str) -> Vec<String> {
    let url = raw.trim().trim_end_matches('/');
    if url.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(stripped) = url.strip_suffix("/dav") {
        out.push(stripped.to_string());
    }
    out.push(url.to_string());
    out.dedup();
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortReq {
    pub method: String,
    pub url: String,
    /// POST 的表单体；GET 为 None
    pub body: Option<String>,
}

/// RFC3986 严格编码：只保留 unreserved 字符。
///
/// 直链里的 `:`、`/`、`?`、`&` 必须编码后放进 query，否则会被短链服务当成自己的参数。
pub fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3 / 2);
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// 组短链请求。参数名按服务方文档：`url` / `format` / `callback` / `apikey`
/// （注意是 `apikey` 而不是 `api_key`，写错服务端只会回一条错误文案）。
pub fn short_request(cfg: &BedCfg, direct: &str) -> Result<ShortReq> {
    let base = cfg.short_url.trim();
    if base.is_empty() {
        bail!("未填写短链请求地址");
    }
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        bail!("短链请求地址必须以 http:// 或 https:// 开头");
    }
    let key = cfg.short_key.trim();
    if key.is_empty() {
        bail!("短链模板一必须填写 api_key");
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    pairs.push(("url".into(), direct.to_string()));
    pairs.push(("apikey".into(), key.to_string()));
    match cfg.short_fmt {
        1 => pairs.push(("format".into(), "json".into())),
        2 => {
            pairs.push(("format".into(), "jsonp".into()));
            let cb = cfg.short_callback.trim();
            pairs.push((
                "callback".into(),
                if cb.is_empty() { "callback".into() } else { cb.to_string() },
            ));
        }
        // 「无」= 服务端直接回纯文本链接，不带 format 参数
        _ => {}
    }
    let encoded = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    if cfg.short_method == 1 {
        Ok(ShortReq {
            method: "POST".into(),
            url: base.to_string(),
            body: Some(encoded),
        })
    } else {
        let sep = if base.contains('?') { '&' } else { '?' };
        Ok(ShortReq {
            method: "GET".into(),
            url: format!("{base}{sep}{encoded}"),
            body: None,
        })
    }
}

/// 截断过长的响应体，避免把整页 HTML 塞进 toast。
fn brief(s: &str) -> String {
    let s = s.trim().replace(['\r', '\n'], " ");
    if s.chars().count() <= 120 {
        s
    } else {
        format!("{}…", s.chars().take(120).collect::<String>())
    }
}

/// 从 JSONP 里剥出 JSON：`cb({...});` → `{...}`；失败时返回 None。
fn split_jsonp(s: &str) -> Option<&str> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    if close <= open + 1 {
        return None;
    }
    Some(s[open + 1..close].trim())
}

fn parse_json_short(s: &str) -> Result<String> {
    let v: Value = serde_json::from_str(s)
        .with_context(|| format!("短链服务返回的 JSON 无法解析: {}", brief(s)))?;
    let err = v.get("err").and_then(|x| x.as_str()).unwrap_or("").trim();
    if !err.is_empty() {
        bail!("短链服务返回错误: {err}");
    }
    let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("").trim();
    if url.is_empty() {
        bail!("短链服务未返回 url: {}", brief(s));
    }
    // 失败时这个服务会把错误文案塞进 url（`status:-1`），必须先按链接形状筛一遍。
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("短链服务未返回链接: {}", brief(url));
    }
    Ok(url.to_string())
}

/// 解析短链响应。三种格式的成功判据不同，失败时把原始响应带回错误信息里。
pub fn parse_short(fmt: usize, body: &str) -> Result<String> {
    let body = body.trim();
    if body.is_empty() {
        bail!("短链服务返回空响应");
    }
    match fmt {
        0 => {
            if body.starts_with("http://") || body.starts_with("https://") {
                Ok(body.lines().next().unwrap_or(body).trim().to_string())
            } else {
                bail!("短链服务没有返回链接: {}", brief(body))
            }
        }
        2 => {
            let inner = split_jsonp(body)
                .ok_or_else(|| anyhow!("JSONP 响应缺少括号包裹: {}", brief(body)))?;
            parse_json_short(inner)
        }
        _ => parse_json_short(body),
    }
}

// ---------------------------------------------------------------- 频率限制

static GATE: Mutex<Option<Instant>> = Mutex::new(None);

/// 「每秒 N 次」排队：返回还需要等多久（0 = 立刻可发）。全局共享，跨多次点击也生效。
pub fn take_slot(per_sec: u32) -> Duration {
    let per = per_sec.max(1) as u64;
    let gap = Duration::from_millis(1000 / per);
    let now = Instant::now();
    let mut g = GATE.lock().unwrap_or_else(|e| e.into_inner());
    let earliest = match *g {
        Some(last) => last + gap,
        None => now,
    };
    let at = if earliest > now { earliest } else { now };
    *g = Some(at);
    at.saturating_duration_since(now)
}

// ---------------------------------------------------------------- 命令行

/// 一次性调用 rclone.exe（不启动服务、不接管进程生命周期）。
pub struct RcloneCli {
    pub exe: PathBuf,
    pub conf: String,
}

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    // GUI 进程没有控制台，子进程默认会闪一个黑框；CREATE_NO_WINDOW 抑制它。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

impl RcloneCli {
    /// 定位 rclone.exe 与配置路径；任意一步失败都只是「不能问 CLI」，不阻断图床流程。
    pub async fn discover(client: &RcClient) -> Option<Self> {
        let exe = discover_exe(None).ok()?;
        let conf = client.config_paths().await.ok()?.config;
        if conf.trim().is_empty() {
            return None;
        }
        Some(Self { exe, conf })
    }

    /// 用已知的 exe / conf 直接构造，供自检使用。
    #[cfg(test)]
    pub fn from_parts(exe: PathBuf, conf: String) -> Self {
        Self { exe, conf }
    }

    /// `rclone link <remote>:<path>`；返回链接或 rclone 自己的失败原因。
    pub async fn link(&self, mount: String) -> Result<String> {
        let exe = self.exe.clone();
        let conf = self.conf.clone();
        tokio::task::spawn_blocking(move || -> Result<String> {
            let mut c = Command::new(&exe);
            c.arg("link")
                .arg(&mount)
                .arg("--config")
                .arg(&conf)
                .arg("--log-level")
                .arg("ERROR");
            no_window(&mut c);
            let out = c.output().context("启动 rclone link 失败")?;
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            if !out.status.success() {
                bail!("{}", first_line(&stderr));
            }
            // teldrive 只输出相对路径（`/share/tds_xxx`），由调用方用远程地址补全；
            // 所以这里接受任意非空行，不做 http 前缀过滤。
            stdout
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow!("rclone link 没有输出链接: {}", brief(&stdout)))
        })
        .await
        .context("rclone link 任务异常")?
    }

    /// `rclone reveal <obscured>`：把 conf 里混淆过的密码还原成明文。
    ///
    /// 只对「本机已配置好的远程」使用，明文只用于当次请求，不落盘。
    pub async fn reveal(&self, obscured: &str) -> Result<String> {
        let exe = self.exe.clone();
        let conf = self.conf.clone();
        let obscured = obscured.to_string();
        tokio::task::spawn_blocking(move || -> Result<String> {
            let mut c = Command::new(&exe);
            c.arg("reveal")
                .arg(&obscured)
                .arg("--config")
                .arg(&conf)
                .arg("--log-level")
                .arg("ERROR");
            no_window(&mut c);
            let out = c.output().context("启动 rclone reveal 失败")?;
            if !out.status.success() {
                bail!(
                    "rclone reveal 失败: {}",
                    first_line(&String::from_utf8_lossy(&out.stderr))
                );
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
        .await
        .context("rclone reveal 任务异常")?
    }
}

// ---------------------------------------------------------------- 远程参数

fn params_map(v: &Value) -> Map<String, Value> {
    // 1.75 的 config/get 直接把参数放在顶层；为兼容嵌套写法再兜一层 `result`/`parameters`。
    let src = v
        .get("result")
        .or_else(|| v.get("parameters"))
        .unwrap_or(v);
    src.as_object().cloned().unwrap_or_default()
}

pub struct RemoteInfo {
    pub kind: String,
    pub url: String,
    pub user: String,
    pub pass: String,
    /// 站点根地址，用来把「相对形式的链接」补成完整链接：
    /// teldrive 的 `rclone link` 只给 `/share/xxx`，需要拿 `api_host` 拼；
    /// 其他后端没有 `api_host` 时退回 `url`。
    pub addr: String,
}

impl RemoteInfo {
    pub fn from_map(m: &Map<String, Value>) -> Self {
        let s = |k: &str| {
            m.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        let addr = ["api_host", "rclone_address", "url"]
            .iter()
            .map(|k| s(k))
            .find(|v| !v.trim().is_empty())
            .unwrap_or_default();
        Self {
            kind: s("type"),
            url: s("url"),
            user: s("user"),
            pass: s("pass"),
            addr,
        }
    }

    /// 把 `rclone link` 的输出补成可点的完整链接。
    ///
    /// 已经是 `http(s)://` 的直接返回；相对路径（teldrive 的 `/share/xxx`）用
    /// [`Self::addr`] 拼；连站点地址都没有时返回 `None`，由调用方给出可读的失败原因。
    pub fn absolute_link(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let lower = raw.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return Some(raw.to_string());
        }
        let base = self.addr.trim().trim_end_matches('/');
        if base.is_empty() {
            return None;
        }
        let base = if base.to_ascii_lowercase().starts_with("http") {
            base.to_string()
        } else {
            format!("https://{base}")
        };
        Some(format!("{}/{}", base, raw.trim_start_matches('/')))
    }

    /// webdav + 有 url：才有可能是 AList（探活会再确认一次，避免把普通 webdav 当 AList）。
    pub fn looks_alist(&self) -> bool {
        self.kind.eq_ignore_ascii_case("webdav") && !self.url.trim().is_empty()
    }
}

pub async fn remote_info(client: &RcClient, name: &str) -> Result<RemoteInfo> {
    let raw = client
        .call("config/get", &json!({ "name": name }))
        .await
        .map_err(|e| anyhow!("读取远程 {name} 的配置失败: {e}"))?;
    Ok(RemoteInfo::from_map(&params_map(&raw)))
}

// ---------------------------------------------------------------- AList 直链

struct Alist {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Alist {
    async fn open(base: &str, user: &str, pass: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(4))
            .timeout(Duration::from_secs(10))
            .build()
            .context("创建 HTTP 客户端失败")?;

        // 1) 探活：AList 的公开设置接口无鉴权，用来确认「这个 webdav 站确实是 AList」。
        let probe = http
            .get(format!("{base}/api/public/settings"))
            .send()
            .await
            .with_context(|| format!("访问 {base}/api/public/settings 失败"))?;
        if !probe.status().is_success() {
            bail!("{base}/api/public/settings 返回 HTTP {}", probe.status().as_u16());
        }
        let body: Value = probe.json().await.unwrap_or(Value::Null);
        // AList 把设置放在 `data` 里（`{"code":200,"data":{"site_title":...}}`）；
        // 兼容某些版本/反代直接平铺在顶层。
        let data = body.get("data").cloned().unwrap_or(Value::Null);
        let pick = |k: &str| {
            data.get(k)
                .or_else(|| body.get(k))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        let title = pick("site_title");
        let version = pick("version");
        if title.is_empty() && version.is_empty() {
            bail!("{base} 不是 AList 站点（public/settings 无 site_title/version）");
        }

        // 2) 登录拿管理员 token（用 rclone conf 里已有的账号密码，不额外存一份凭据）。
        let resp = http
            .post(format!("{base}/api/auth/login"))
            .json(&json!({ "username": user, "password": pass }))
            .send()
            .await
            .context("AList 登录请求失败")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let message = body
            .get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("未知错误")
            .to_string();
        if !status.is_success() {
            bail!("AList 登录失败（HTTP {}）：{message}", status.as_u16());
        }
        // AList 成功也带 `code`；非 200 时 token 一定拿不到，先把 message 报出来。
        let code = body.get("code").and_then(|x| x.as_i64()).unwrap_or(200);
        if code != 200 {
            bail!("AList 登录返回 code={code}：{message}");
        }
        // token 在 `data.token`（v3 全系）；老版本/反代可能平铺在顶层。
        let data = body.get("data").cloned().unwrap_or(Value::Null);
        let token = data
            .get("token")
            .or_else(|| body.get("token"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if token.is_empty() {
            bail!("AList 登录成功但没有返回 token（响应：{}）", brief(&body.to_string()));
        }
        Ok(Self { http, base: base.to_string(), token })
    }

    /// `/api/fs/get` 返回的 `data.raw_url` 就是存储后端的直链（可能带签名、会过期）。
    async fn direct(&self, path: &str) -> Result<(String, String)> {
        // AList 的 fs 接口只认以 `/` 开头的绝对路径；图床里存的 `img/a.png` 得补成 `/img/a.png`。
        let abs = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        let resp = self
            .http
            .post(format!("{}/api/fs/get", self.base))
            .header("Authorization", &self.token)
            .json(&json!({ "path": abs, "password": "" }))
            .send()
            .await
            .context("AList fs/get 请求失败")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let msg = body
                .get("message")
                .and_then(|x| x.as_str())
                .unwrap_or("未知错误");
            bail!("AList fs/get {abs} 失败（HTTP {}）：{msg}", status.as_u16());
        }
        let code = body.get("code").and_then(|x| x.as_i64()).unwrap_or(0);
        if code != 200 {
            let msg = body
                .get("message")
                .and_then(|x| x.as_str())
                .unwrap_or("未知错误");
            bail!("AList fs/get {abs} 返回 code={code}：{msg}");
        }
        let data = body.get("data").cloned().unwrap_or(Value::Null);
        let raw = data
            .get("raw_url")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if raw.is_empty() {
            bail!("AList 没有给出 {abs} 的直链（该驱动可能不支持 raw_url）");
        }
        // provider 名字只用于在提示里说明「直链来自哪个存储」。
        let provider = data
            .get("provider")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        Ok((raw, provider))
    }
}

/// 直链来源，用来在 UI 里说明「这条链接是怎么来的」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// rclone link（后端给的公开/临时链接）
    Cli,
    /// rclone link 返回的是相对地址，补全后是一个「分享页」而不是文件本身（teldrive 就是这样）
    Share,
    /// AList 管理 API 的 raw_url
    Alist,
    /// K-Vault 站点 API 返回的签名直链（`links.download`）
    Kvault,
    /// 本机 HTTP 代理
    Proxy,
}

impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Source::Cli => "rclone link",
            Source::Share => "rclone link（分享页）",
            Source::Alist => "AList raw_url",
            Source::Kvault => "K-Vault 站点直链",
            Source::Proxy => "本机代理",
        }
    }

    pub fn note(&self) -> &'static str {
        match self {
            Source::Cli => "后端公开链接（是否长期有效取决于后端）",
            Source::Share => "这是后端分享页（网页预览），不是图片文件直链",
            Source::Alist => "存储后端直链，可能带签名、会过期",
            Source::Kvault => "站点签名的下载直链；站点换 Telegram bot token 后会全部失效",
            Source::Proxy => "只有本机能打开（127.0.0.1）",
        }
    }
}

/// 依次尝试三种来源，返回第一条成功的链接与来源。
///
/// `proxy` 为主程序已拼好的本机代理链接（用户没配存储根时为空串）。
pub async fn resolve_link(
    cli: Option<&RcloneCli>,
    info: &RemoteInfo,
    mount: &str,
    stored: &str,
    proxy: &str,
) -> Result<(String, Source)> {
    let mut attempts: Vec<String> = Vec::new();

    if let Some(cli) = cli {
        match cli.link(mount.to_string()).await {
            Ok(raw) => match info.absolute_link(&raw) {
                Some(u) => {
                    // 相对地址 = 该后端只给「分享路径」（teldrive 的 /share/xxx），补全后是网页。
                    let relative = raw.trim().starts_with('/');
                    let src = if relative && info.kind.eq_ignore_ascii_case("teldrive") {
                        Source::Share
                    } else {
                        Source::Cli
                    };
                    return Ok((u, src));
                }
                None => attempts.push(format!(
                    "rclone link 只给出相对链接 {raw}，而远程 {} 没有配置站点地址（api_host / url），无法补全",
                    if info.kind.is_empty() { "未知" } else { &info.kind }
                )),
            },
            Err(e) => attempts.push(format!("rclone link：{e:#}")),
        }
    } else {
        attempts.push("rclone link：找不到 rclone.exe 或 rclone.conf".into());
    }

    if info.looks_alist() {
        let candidates = alist_base_candidates(&info.url);
        let mut pass = info.pass.clone();
        if !pass.is_empty() {
            if let Some(cli) = cli {
                match cli.reveal(&pass).await {
                    Ok(p) => pass = p,
                    Err(e) => attempts.push(format!("rclone reveal：{e:#}")),
                }
            }
        }
        // 两个候选地址都试；每个候选的失败原因都留下来，避免最后一条把前一条盖掉。
        let mut why: Vec<String> = Vec::new();
        let mut got = None;
        for base in &candidates {
            match Alist::open(base, &info.user, &pass).await {
                Ok(a) => match a.direct(stored).await {
                    Ok((url, provider)) => {
                        got = Some((url, provider));
                        break;
                    }
                    Err(e) => why.push(format!("{base}：{e:#}")),
                },
                Err(e) => why.push(format!("{base}：{e:#}")),
            }
        }
        match got {
            Some((url, _provider)) => return Ok((url, Source::Alist)),
            None => attempts.push(format!("AList：{}", why.join("；"))),
        }
    } else {
        attempts.push(format!(
            "AList 直链：远程类型是 {}，只有 webdav 类型的 AList 远程能走管理 API",
            if info.kind.is_empty() { "未知" } else { &info.kind }
        ));
    }

    if !proxy.trim().is_empty() {
        return Ok((proxy.to_string(), Source::Proxy));
    }

    bail!("无法得到直链：\n- {}", attempts.join("\n- "))
}

// ---------------------------------------------------------------- 短链请求

/// 发一次短链请求并解析。频率控制在调用方（[`take_slot`]）做。
pub async fn request_short(cfg: &BedCfg, direct: &str) -> Result<String> {
    let req = short_request(cfg, direct)?;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .context("创建 HTTP 客户端失败")?;
    let builder = match req.method.as_str() {
        "POST" => {
            let body = req.body.clone().unwrap_or_default();
            http.post(&req.url)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(body)
        }
        _ => http.get(&req.url),
    };
    let resp = builder.send().await.context("请求短链服务失败")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("短链服务返回 HTTP {}：{}", status.as_u16(), brief(&body));
    }
    parse_short(cfg.short_fmt, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_stored_path() {
        assert_eq!(join_stored("img", "a.png"), "img/a.png");
        assert_eq!(join_stored("/img/", "/a.png"), "img/a.png");
        assert_eq!(join_stored("", "a.png"), "a.png");
        assert_eq!(join_stored("/", "a.png"), "a.png");
        assert_eq!(join_stored("a/b", "c.png"), "a/b/c.png");
    }

    #[test]
    fn mounts_remote() {
        assert_eq!(mount_path("alist", "img/a.png"), "alist:img/a.png");
        assert_eq!(mount_path("alist:", "img/a.png"), "alist:img/a.png");
        assert_eq!(mount_path(" alist ", "a.png"), "alist:a.png");
    }

    #[test]
    fn history_keeps_newest_first_without_duplicates() {
        let mut cfg = BedCfg::default();
        cfg.remember_path("img");
        cfg.remember_path("/photo/");
        // 重复用的路径回到最前，而不是再存一条
        cfg.remember_path("img");
        assert_eq!(cfg.path_history, vec!["img", "photo"]);
        // 空路径也要留一条：远程根（留空）是常用的选择，显示成 `/`
        cfg.remember_path("");
        assert_eq!(cfg.path_history[0], "/");
        for i in 0..20 {
            cfg.remember_path(&format!("p{i}"));
        }
        assert_eq!(cfg.path_history.len(), HISTORY_MAX);
        assert_eq!(cfg.path_history[0], "p19");
    }

    #[test]
    fn dedupe_defaults_to_on_and_survives_old_config() {
        // 老配置文件里没有这两个字段：反序列化后去重默认开、历史为空
        let cfg: BedCfg = serde_json::from_str(r#"{"remote":"k-vault"}"#).unwrap();
        assert!(cfg.dedupe);
        assert!(cfg.path_history.is_empty());
    }

    #[test]
    fn alist_candidates_strip_dav() {
        assert_eq!(
            alist_base_candidates("https://h.example/dav/"),
            vec!["https://h.example".to_string(), "https://h.example/dav".to_string()]
        );
        assert_eq!(
            alist_base_candidates("https://h.example"),
            vec!["https://h.example".to_string()]
        );
        assert!(alist_base_candidates("  ").is_empty());
    }

    #[test]
    fn encodes_reserved_chars() {
        assert_eq!(form_encode("a b"), "a%20b");
        assert_eq!(
            form_encode("https://h/p/a.png?x=1&y=2"),
            "https%3A%2F%2Fh%2Fp%2Fa.png%3Fx%3D1%26y%3D2"
        );
    }

    #[test]
    fn builds_jsonp_request() {
        let cfg = BedCfg {
            short_url: "http://api.ft12.com/api.php".into(),
            short_fmt: 2,
            short_callback: "cb".into(),
            short_method: 0,
            short_key: "k".into(),
            ..Default::default()
        };
        let r = short_request(&cfg, "https://h/a.png").unwrap();
        assert_eq!(r.method, "GET");
        assert!(r.url.starts_with("http://api.ft12.com/api.php?"));
        assert!(r.url.contains("format=jsonp"));
        assert!(r.url.contains("callback=cb"));
        assert!(r.url.contains("apikey=k"));
        assert!(r.url.contains("url=https%3A%2F%2Fh%2Fa.png"));
        assert!(r.body.is_none());
    }

    #[test]
    fn builds_plain_post_request() {
        let cfg = BedCfg {
            short_url: "https://s.example/api".into(),
            short_fmt: 0,
            short_method: 1,
            short_key: "k".into(),
            ..Default::default()
        };
        let r = short_request(&cfg, "https://h/a.png").unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.url, "https://s.example/api");
        let body = r.body.unwrap();
        assert!(body.contains("url=https%3A%2F%2Fh%2Fa.png"));
        assert!(body.contains("apikey=k"));
        // 「无」= 纯文本格式：不带 format / callback
        assert!(!body.contains("format="));
        assert!(!body.contains("callback="));
    }

    #[test]
    fn requires_api_key() {
        let cfg = BedCfg {
            short_url: "https://s.example/api".into(),
            ..Default::default()
        };
        let e = short_request(&cfg, "https://h/a.png").unwrap_err();
        assert!(format!("{e:#}").contains("api_key"));
    }

    #[test]
    fn rejects_bad_url() {
        let cfg = BedCfg {
            short_url: "api.ft12.com/api.php".into(),
            ..Default::default()
        };
        assert!(short_request(&cfg, "https://h/a.png").is_err());
        let cfg2 = BedCfg::default();
        assert!(short_request(&cfg2, "https://h/a.png").is_err());
    }

    #[test]
    fn parses_plain_text() {
        assert_eq!(parse_short(0, " https://s.cn/abc \n").unwrap(), "https://s.cn/abc");
        assert!(parse_short(0, "<html>bad</html>").is_err());
    }

    #[test]
    fn parses_json_ok_and_err() {
        assert_eq!(
            parse_short(1, r#"{"url":"https://s.cn/x","err":""}"#).unwrap(),
            "https://s.cn/x"
        );
        let e = parse_short(1, r#"{"url":"","err":"额度已用完"}"#).unwrap_err();
        assert!(format!("{e:#}").contains("额度已用完"));
        assert!(parse_short(1, r#"{"url":""}"#).is_err());
        assert!(parse_short(1, "not json").is_err());
        // 失败时服务端把错误文案塞进 url、err 为空：不能当成链接返回
        let e = parse_short(1, r#"{"status":-1,"url":"API错误, 请登录后重新获取","err":""}"#)
            .unwrap_err();
        assert!(format!("{e:#}").contains("未返回链接"));
    }

    #[test]
    fn parses_jsonp() {
        assert_eq!(
            parse_short(2, r#"callback({"url":"https://s.cn/y","err":""});"#).unwrap(),
            "https://s.cn/y"
        );
        assert!(parse_short(2, r#"{"url":"https://s.cn/y"}"#).is_err());
    }

    #[test]
    fn rate_limiter_spaces_requests() {
        // 先在测试线程里占一个槽，再连续取两次看间隔是否 >= 1000/rate。
        let first = take_slot(4);
        assert!(first.is_zero(), "第一次不应等待");
        let second = take_slot(4);
        assert!(second >= Duration::from_millis(200), "第二次应等待约 250ms，实际 {second:?}");
    }

    #[test]
    fn remote_info_detects_alist() {
        let m = json!({"type": "webdav", "url": "https://h/dav", "user": "u", "pass": "p"});
        let info = RemoteInfo::from_map(m.as_object().unwrap());
        assert!(info.looks_alist());
        let m2 = json!({"type": "s3", "url": "https://h"});
        let info2 = RemoteInfo::from_map(m2.as_object().unwrap());
        assert!(!info2.looks_alist());
    }

    #[test]
    fn joins_relative_links() {
        // teldrive：rclone link 只给 `/share/xxx`，要用 api_host 补全
        let m = json!({"type": "teldrive", "api_host": "https://t.example/"});
        let info = RemoteInfo::from_map(m.as_object().unwrap());
        assert_eq!(
            info.absolute_link("/share/tds_x").as_deref(),
            Some("https://t.example/share/tds_x")
        );
        // 已经完整的链接原样返回
        assert_eq!(
            info.absolute_link("https://cdn.example/a.png?sign=1").as_deref(),
            Some("https://cdn.example/a.png?sign=1")
        );
        // api_host 没写协议时补 https://
        let m2 = json!({"type": "teldrive", "api_host": "t.example"});
        let info2 = RemoteInfo::from_map(m2.as_object().unwrap());
        assert_eq!(
            info2.absolute_link("share/x").as_deref(),
            Some("https://t.example/share/x")
        );
        // 没有站点地址 + 相对链接：交给调用方报错，而不是拼出坏链
        let m3 = json!({"type": "webdav"});
        let info3 = RemoteInfo::from_map(m3.as_object().unwrap());
        assert_eq!(info3.absolute_link("/share/x"), None);
        // webdav 远程退回 url 作为站点地址
        let m4 = json!({"type": "webdav", "url": "https://alist.example/dav"});
        let info4 = RemoteInfo::from_map(m4.as_object().unwrap());
        assert_eq!(
            info4.absolute_link("/share/x").as_deref(),
            Some("https://alist.example/dav/share/x")
        );
    }

    #[test]
    fn params_map_unwraps_result() {
        let v = json!({"result": {"type": "webdav", "url": "https://h"}});
        let m = params_map(&v);
        assert_eq!(m.get("type").and_then(|x| x.as_str()), Some("webdav"));
    }

    #[test]
    fn source_labels_exist() {
        assert_eq!(Source::Alist.label(), "AList raw_url");
        assert_eq!(Source::Share.label(), "rclone link（分享页）");
        assert!(Source::Proxy.note().contains("127.0.0.1"));
    }

    // ------------------------------------------------------------ 真机自检（默认忽略）
    //
    // 这两条会真连本机 rclone.conf 里的远程，只在手动排查时跑：
    //   cargo test -p rclone-gui bed_live_ -- --ignored --nocapture
    //
    // `bed_live_alist_open` 守的是「AList 探活把 site_title/version 当顶层字段」那个 bug；
    // `bed_live_teldrive_link` 守的是「teldrive 的 rclone link 只给相对地址」那个 bug。

    fn live_cli() -> Option<RcloneCli> {
        let exe = discover_exe(None).ok()?;
        let out = std::process::Command::new(&exe)
            .arg("config")
            .arg("file")
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let conf = text.lines().map(str::trim).filter(|l| !l.is_empty()).last()?.to_string();
        Some(RcloneCli::from_parts(exe, conf))
    }

    fn live_dump(cli: &RcloneCli) -> Value {
        let out = std::process::Command::new(&cli.exe)
            .args(["config", "dump"])
            .output()
            .expect("rclone config dump 失败");
        serde_json::from_slice(&out.stdout).expect("config dump 不是 JSON")
    }

    #[test]
    #[ignore = "需要本机已配置 alist 远程（真网络）"]
    fn bed_live_alist_open() {
        let cli = live_cli().expect("找不到 rclone.exe / rclone.conf");
        let dump = live_dump(&cli);
        let raw = dump.get("alist").expect("本机没有名为 alist 的远程");
        let info = RemoteInfo::from_map(raw.as_object().unwrap());
        assert!(info.looks_alist(), "alist 远程应是 webdav 且有 url");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let bases = alist_base_candidates(&info.url);
        let pass = rt.block_on(cli.reveal(&info.pass)).expect("rclone reveal 失败");
        let mut errs = Vec::new();
        let mut ok = None;
        for base in &bases {
            match rt.block_on(Alist::open(base, &info.user, &pass)) {
                Ok(a) => {
                    ok = Some(base.clone());
                    // 顺手确认真能拿到直链：根目录一定是目录，raw_url 为空是正常的，
                    // 这里只要求接口 code=200（失败会返回 Err）。
                    let _ = rt.block_on(a.direct("/"));
                    break;
                }
                Err(e) => errs.push(format!("{base}：{e:#}")),
            }
        }
        assert!(ok.is_some(), "AList 探活/登录全部失败：\n{}", errs.join("\n"));
        println!("bed_live_alist_open ok via {}", ok.unwrap());
    }

    #[test]
    #[ignore = "需要本机已配置 teldrive 远程（真网络）"]
    fn bed_live_teldrive_link() {
        let cli = live_cli().expect("找不到 rclone.exe / rclone.conf");
        let dump = live_dump(&cli);
        let raw = dump.get("teldrive").expect("本机没有名为 teldrive 的远程");
        let info = RemoteInfo::from_map(raw.as_object().unwrap());
        assert!(!info.addr.is_empty(), "teldrive 远程应有 api_host");

        // 找一个真实文件做样本
        let out = std::process::Command::new(&cli.exe)
            .args(["lsf", "teldrive:", "--max-depth", "2"])
            .output()
            .expect("rclone lsf 失败");
        let listing = String::from_utf8_lossy(&out.stdout).to_string();
        let file = listing
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.ends_with('/'))
            .expect("teldrive 里没有可用文件")
            .to_string();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mount = mount_path("teldrive", &file);
        let got = rt.block_on(resolve_link(Some(&cli), &info, &mount, &file, ""));
        let (url, src) = got.expect("teldrive 取直链失败");
        println!("bed_live_teldrive_link ok: {url} via {}", src.label());
        assert!(url.starts_with("http"), "链接应以 http 开头：{url}");
        assert_eq!(src, Source::Share, "teldrive 走的是分享页");
    }
}
