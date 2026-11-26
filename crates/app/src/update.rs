//! 检查并安装官方更新（GitHub Releases）。
//!
//! 这个模块只做两件事：从官方仓库读发布清单、把校验过的文件换到本地。它不负责
//! 重启后的清理，也不碰用户数据。
//!
//! 三条底线：
//!
//! 1. **只认官方仓库的固定文件名**。版本、标签、文件名三者必须互相对得上，地址
//!    必须落在 `https://github.com/h897105167/rclone_gui_windows/releases/download/<tag>/<file>`
//!    上；不接受明文、不带端口的例外、不接受查询串和重定向到别处。
//! 2. **先校验再落盘，先备份再替换**。清单和 SHA256 都对上了才下载；所有要改的
//!    文件先逐份备份并核对哈希，任何一份备份失败就一个字节都不动。替换中途失败
//!    会按备份逐项还原。
//! 3. **不碰用户数据**。便携版的替换清单只含程序白名单里的文件（`rclone-gui.exe`、
//!    `core/`、`assets/`、`plugins/`、`licenses/`、`portable/`），`data/` 和配置
//!    文件永远不在清单里；备份目录放在系统临时目录，不在程序目录内部。
//!
//! 替换本身不能在本进程里做（要替换的正是正在运行的 exe），所以 `apply` 生成一份
//! JSON 计划和一个 PowerShell 脚本，脚本用 `-LiteralPath` 逐项读计划里的路径 ——
//! 路径从不拼进命令行字符串，含空格、`%`、`'`、中文的目录不会被二次解释。

use anyhow::{bail, Context as _, Result};
use rcgui_i18n::{f as msg, t};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// 官方仓库（owner/name）。发布资产只从这里取。
pub const REPO: &str = "h897105167/rclone_gui_windows";

/// 单个发布资产的大小上限。便携包解开前约 40 MiB，160 MiB 留足余量，
/// 同时保证一个伪造的清单不能让进程把内存吃光。
pub const MAX_ASSET_BYTES: u64 = 160 * 1024 * 1024;

/// 发布清单（各资产 sha256）的文件名。
pub const SUMS_NAME: &str = "SHA256SUMS";

/// 便携包解压后的总字节上限。故意不用 `plugin_host::package` 的 4 GB 全局预算：
/// 那套预算是给插件包定的，套到自更新上等于没有上限。
const PORTABLE_MAX_EXTRACTED_BYTES: u64 = 512 * 1024 * 1024;

/// 发布清单和 API 响应的读取上限。
const MANIFEST_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// 允许直连的主机。
const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "api.github.com",
    "objects.githubusercontent.com",
    "codeload.github.com",
    "release-assets.githubusercontent.com",
];

/// 便携版允许出现在包根的顶级名字。白名单之外一律拒绝 —— 多一个 `data/` 或
/// 一份 `rcgui.conf` 就足以覆盖用户数据。
const PORTABLE_TOP: &[&str] = &[
    "rclone-gui.exe",
    "core",
    "assets",
    "plugins",
    "licenses",
    "portable",
    SUMS_NAME,
];

// ---------------- 单实例 ----------------

static BUSY: AtomicBool = AtomicBool::new(false);

/// 占住"正在检查/安装更新"。返回的值 drop 时自动释放。
pub struct Guard(());

/// 尝试占住更新流程；已有一次在跑时返回 `None`。
pub fn try_acquire() -> Option<Guard> {
    BUSY.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .ok()
        .map(|_| Guard(()))
}

impl Drop for Guard {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::SeqCst);
    }
}

// ---------------- 版本号 ----------------

/// 安装包形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 安装到固定目录的安装器。
    Setup,
    /// 免安装的 zip。
    Portable,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Setup => "setup",
            Kind::Portable => "portable",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "setup" | "installer" | "exe" => Some(Kind::Setup),
            "portable" | "zip" => Some(Kind::Portable),
            _ => None,
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Kind::Setup => "-windows-x64-setup.exe",
            Kind::Portable => "-windows-x64-portable.zip",
        }
    }
}

/// 本程序编译时的版本号。
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 资产文件名：`RCGUI-<版本>-windows-x64-setup.exe` / `-portable.zip`。
pub fn asset_name(version: &str, kind: Kind) -> String {
    let v = version.trim().trim_start_matches(['v', 'V']);
    format!("RCGUI-{v}{}", kind.suffix())
}

/// 解析出来的版本号。`pre` 是预发布标识段（`1.0.0-rc.1` 的 `["rc","1"]`）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Semver {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<String>,
}

/// 按 semver.org 解析。构建元数据（`+` 之后）不参与比较，解析后丢弃。
///
/// 允许 `v` 前缀和缺省字段（`1.2` 视作 `1.2.0`），但主干超过三段就判为不是版本号。
fn parse_semver(s: &str) -> Option<Semver> {
    let s = s.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let s = match s.find('+') {
        Some(i) => &s[..i],
        None => s,
    };
    let (core, pre) = match s.find('-') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let num = |x: &str| -> Option<u64> {
        if x.is_empty() || !x.bytes().all(|b| b.is_ascii_digit()) {
            None
        } else {
            x.parse().ok()
        }
    };
    let major = num(parts[0])?;
    let minor = if parts.len() > 1 { num(parts[1])? } else { 0 };
    let patch = if parts.len() > 2 { num(parts[2])? } else { 0 };
    let pre = match pre {
        None => Vec::new(),
        Some(p) => {
            if p.is_empty() {
                return None;
            }
            let segs: Vec<String> = p.split('.').map(|x| x.to_string()).collect();
            if segs
                .iter()
                .any(|x| x.is_empty() || !x.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
            {
                return None;
            }
            segs
        }
    };
    Some(Semver {
        major,
        minor,
        patch,
        pre,
    })
}

fn cmp_num(a: u64, b: u64) -> std::cmp::Ordering {
    a.cmp(&b)
}

/// 预发布段比较：两边都是数字就按数值比；数字段总是低于字母段；前缀相同则长的一方更大。
fn cmp_pre(a: &[String], b: &[String]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.is_empty(), b.is_empty()) {
        // 有预发布标识的版本低于同号的正式版（1.0.0-rc.1 < 1.0.0）
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for (x, y) in a.iter().zip(b.iter()) {
        let nx = x.bytes().all(|c| c.is_ascii_digit());
        let ny = y.bytes().all(|c| c.is_ascii_digit());
        let ord = match (nx, ny) {
            (true, true) => {
                let (xi, yi): (u64, u64) = (x.parse().unwrap_or(0), y.parse().unwrap_or(0));
                cmp_num(xi, yi)
            }
            // 数字标识低于字母标识
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// `latest` 是否比 `current` 新。任一边解析不出来都返回 false（宁可没提示，
/// 不要提示一个装不上的版本）。
pub fn is_newer(latest: &str, current: &str) -> bool {
    let (Some(a), Some(b)) = (parse_semver(latest), parse_semver(current)) else {
        return false;
    };
    match (a.major, a.minor, a.patch).cmp(&(b.major, b.minor, b.patch)) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => cmp_pre(&a.pre, &b.pre) == std::cmp::Ordering::Greater,
    }
}

// ---------------- 地址校验 ----------------

/// 把 `scheme://authority/path?query#frag` 拆开，只接受能看懂的写法。
struct Parts {
    https: bool,
    host: String,
    port: Option<u16>,
    userinfo: bool,
    path: String,
    has_query: bool,
    has_fragment: bool,
}

fn split_url(url: &str) -> Option<Parts> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    let (https, rest) = if lower.starts_with("https://") {
        (true, &u[8..])
    } else if lower.starts_with("http://") {
        (false, &u[7..])
    } else {
        return None;
    };
    let (before_frag, has_fragment) = match rest.find('#') {
        Some(i) => (&rest[..i], true),
        None => (rest, false),
    };
    let (before_query, has_query) = match before_frag.find('?') {
        Some(i) => (&before_frag[..i], true),
        None => (before_frag, false),
    };
    let auth_end = before_query.find('/').unwrap_or(before_query.len());
    let authority = &before_query[..auth_end];
    let path = &before_query[auth_end..];
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (true, &authority[i + 1..]),
        None => (false, authority),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => {
            if p.is_empty() {
                (h, None)
            } else {
                (h, p.parse::<u16>().ok())
            }
        }
        None => (hostport, None),
    };
    Some(Parts {
        https,
        host: host.to_ascii_lowercase(),
        port,
        userinfo,
        path: path.to_string(),
        has_query,
        has_fragment,
    })
}

/// 资产地址的形状要求：HTTPS、官方主机、443、无凭据、无查询串。
pub fn check_asset_url(url: &str) -> Result<()> {
    let p = split_url(url)
        .ok_or_else(|| anyhow::anyhow!("{}", msg("不是可识别的地址：{}", &[&url])))?;
    if !p.https {
        bail!("{}", msg("更新地址必须是 https：{}", &[&url]));
    }
    if p.userinfo {
        bail!("{}", msg("更新地址不能带用户名密码：{}", &[&url]));
    }
    if !ALLOWED_HOSTS.contains(&p.host.as_str()) {
        bail!("{}", msg("更新地址不在允许的主机里：{}", &[&(p.host)]));
    }
    if let Some(port) = p.port {
        if port != 443 {
            bail!("{}", msg("更新地址只能用 443 端口：{}", &[&(p.host)]));
        }
    }
    if p.has_query || p.has_fragment {
        bail!("{}", msg("更新地址不能带查询串：{}", &[&(p.host)]));
    }
    Ok(())
}

/// 资产地址还必须指向**这个**仓库的**这个**标签下的**这个**文件。
fn check_release_asset_url(url: &str, tag: &str, name: &str) -> Result<()> {
    check_asset_url(url)?;
    let p = split_url(url).expect("已通过 check_asset_url");
    if p.host != "github.com" {
        bail!("{}", msg("发布包只能从 github.com 下载：{}", &[&(p.host)]));
    }
    let want = format!("/{REPO}/releases/download/{tag}/{name}");
    let got = p.path.trim_end_matches('/');
    // 逐段比较，避免 `/x/releases/download/v1/a.exe` 用结尾相同蒙混过关。
    if got != want {
        bail!(
            "{}",
            msg("发布包地址和期望不符：期望 {}，实际 {}", &[&(want), &(got)])
        );
    }
    Ok(())
}

/// 从 `SHA256SUMS` 里取某个文件名的哈希（64 位十六进制，统一小写）。
///
/// 兼容 `hash  name` 和 `hash *name`（`sha256sum` 的二进制模式标记）。
pub fn sha_for_name(sums: &str, name: &str) -> Option<String> {
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let (Some(hash), Some(file)) = (it.next(), it.next()) else {
            continue;
        };
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let file = file.trim_start_matches('*');
        if file == name {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// 本模块自己算的 sha256，不依赖 `crate::core`（那个模块目前没被声明）。
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------- 发布清单 ----------------

#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    pub version: String,
    pub notes: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub notes: String,
    pub kind: Kind,
    pub asset: Asset,
    pub sums_url: String,
    pub sums_name: String,
}

/// 解析 `releases/latest` 的 JSON。清单本身不可信，这里把所有约束一次查完：
/// 版本要比当前新、资产名必须精确匹配、资产地址必须指向本仓库。
pub fn parse_release(text: &str, current: &str, kind: Kind) -> Result<UpdateInfo> {
    let v: Value = serde_json::from_str(text).with_context(|| t("发布清单不是合法 JSON"))?;
    if let Some(api_error) = v.get("message").and_then(|m| m.as_str()) {
        if v.get("tag_name").is_none() {
            bail!("{}", msg("检查更新失败：{}", &[&api_error]));
        }
    }
    let tag = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if tag.is_empty() {
        bail!("{}", t("发布清单缺少 tag_name"));
    }
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    if parse_semver(&version).is_none() {
        bail!("{}", msg("发布标签不是版本号：{}", &[&tag]));
    }
    if !is_newer(&version, current) {
        bail!("{}", msg("已经是最新版本（{}）", &[&current]));
    }
    let notes = v
        .get("body")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let mut assets: Vec<Asset> = Vec::new();
    if let Some(list) = v.get("assets").and_then(|x| x.as_array()) {
        for a in list {
            let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let url = a
                .get("browser_download_url")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if name.is_empty() || url.is_empty() {
                continue;
            }
            assets.push(Asset {
                name: name.to_string(),
                url: url.to_string(),
                size: a.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
            });
        }
    }
    let want = asset_name(&version, kind);
    let asset = assets
        .iter()
        .find(|a| a.name == want)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{}", msg("发布里没有 {} 这个文件", &[&want])))?;
    check_release_asset_url(&asset.url, &tag, &want)?;
    if asset.size > MAX_ASSET_BYTES {
        bail!(
            "{}",
            msg(
                "发布包 {} 字节，超过上限 {}",
                &[&(asset.size), &MAX_ASSET_BYTES]
            )
        );
    }
    let sums = assets
        .iter()
        .find(|a| a.name == SUMS_NAME)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{}", msg("发布里没有 {}", &[&SUMS_NAME])))?;
    check_release_asset_url(&sums.url, &tag, SUMS_NAME)?;
    Ok(UpdateInfo {
        current: current.to_string(),
        latest: version,
        notes,
        kind,
        asset,
        sums_url: sums.url,
        sums_name: sums.name,
    })
}

// ---------------- 下载 ----------------

/// 跳转目标能不能跟：必须仍是 HTTPS 的 443 端口、官方主机、不带用户名，
/// 并且跳转次数有限。降级成明文或换到别的域名都会把请求（含查询参数）送给别人。
fn redirect_ok(url: &reqwest::Url, hops: usize) -> bool {
    hops < 5
        && url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && ALLOWED_HOSTS.contains(&url.host_str().unwrap_or(""))
        && url.username().is_empty()
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(10))
        // 只放行官方主机；HTTPS 之外的跳转（降级成明文）一律拒绝。
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if redirect_ok(attempt.url(), attempt.previous().len()) {
                attempt.follow()
            } else {
                let bad = attempt.url().as_str().to_owned();
                attempt.error(anyhow::anyhow!(
                    "{}",
                    msg("下载被重定向到不允许的地址：{}", &[&bad])
                ))
            }
        }))
        .build()
        .context(t("创建 HTTP 客户端失败"))
}

/// 边下边数，超过 `cap` 立刻停手。不信任 `Content-Length`：分块响应根本没有它。
async fn fetch_capped(url: &str, cap: u64) -> Result<Vec<u8>> {
    let resp = client()?
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            format!("RCGUI/{}", current_version()),
        )
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("{}", msg("连接 {} 失败：{}", &[&url, &e.to_string()])))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("{}", msg("{} 返回 {}", &[&url, &status.as_u16()]));
    }
    if let Some(len) = resp.content_length() {
        if len > cap {
            bail!(
                "{}",
                msg("{} 声明 {} 字节，超过上限 {}", &[&url, &(len), &cap])
            );
        }
    }
    let mut out: Vec<u8> = Vec::new();
    let mut resp = resp;
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| anyhow::anyhow!("{}", msg("下载 {} 中断：{}", &[&url, &e.to_string()])))?;
        let Some(chunk) = chunk else {
            break;
        };
        if out.len() as u64 + chunk.len() as u64 > cap {
            bail!("{}", msg("{} 超过上限 {} 字节，已中止", &[&url, &cap]));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// 问官方仓库"最新发布是什么"，只返回比 `current` 新的结果。
pub async fn check(current: &str, kind: Kind) -> Result<UpdateInfo> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = fetch_capped(&url, MANIFEST_MAX_BYTES).await?;
    let text = String::from_utf8(body).context(t("发布清单不是 UTF-8"))?;
    parse_release(&text, current, kind)
}

// ---------------- 校验与解包 ----------------

/// 已经下载好、校验过的包。
#[derive(Debug, Clone)]
pub struct Staged {
    pub kind: Kind,
    pub version: String,
    /// 下载下来的原始文件（安装器 .exe 或便携 .zip）。
    pub package: PathBuf,
    /// 便携版解压并校验后的程序根；安装器为 `None`。
    pub unpacked: Option<PathBuf>,
}

/// 下载 + 校验 + （便携版）解包到 `staging_dir`。
///
/// 顺序故意是：先取清单 → 从清单里拿期望哈希 → 再下载 → 比对哈希。哈希对不上就
/// 不留下任何可执行文件。
pub async fn stage(info: &UpdateInfo, staging_dir: &Path) -> Result<Staged> {
    if info.asset.name != asset_name(&info.latest, info.kind) {
        bail!("{}", msg("资产名和版本对不上：{}", &[&(info.asset.name)]));
    }
    if info.asset.name.contains(['/', '\\', ':']) || info.asset.name.contains("..") {
        bail!(
            "{}",
            msg("资产名不是单纯的文件名：{}", &[&(info.asset.name)])
        );
    }
    // 清单可能是别处拼出来的，下载前再验一次地址；只有官方主机、443、无用户名、无查询串。
    check_asset_url(&info.asset.url)?;
    check_asset_url(&info.sums_url)?;
    std::fs::create_dir_all(staging_dir)
        .with_context(|| msg("创建目录 {} 失败", &[&(staging_dir.display())]))?;

    let sums = fetch_capped(&info.sums_url, MANIFEST_MAX_BYTES).await?;
    let sums = String::from_utf8(sums).context(t("SHA256SUMS 不是 UTF-8"))?;
    let expect = sha_for_name(&sums, &info.asset.name).ok_or_else(|| {
        anyhow::anyhow!("{}", msg("{} 里没有 {}", &[&SUMS_NAME, &(info.asset.name)]))
    })?;

    let bytes = fetch_capped(&info.asset.url, MAX_ASSET_BYTES).await?;
    let got = sha256_hex(&bytes);
    if got != expect {
        bail!(
            "{}",
            msg(
                "{} 的 sha256 不符（清单 {}，实际 {}），已丢弃",
                &[&(info.asset.name), &expect, &got]
            )
        );
    }
    let package = staging_dir.join(&info.asset.name);
    std::fs::write(&package, &bytes)
        .with_context(|| msg("写入 {} 失败", &[&(package.display())]))?;

    let unpacked = match info.kind {
        Kind::Setup => None,
        Kind::Portable => {
            let dir = staging_dir.join("unpacked");
            if dir.exists() {
                std::fs::remove_dir_all(&dir)
                    .with_context(|| msg("清理 {} 失败", &[&(dir.display())]))?;
            }
            std::fs::create_dir_all(&dir)
                .with_context(|| msg("创建目录 {} 失败", &[&(dir.display())]))?;
            plugin_host::zip::extract_all(&bytes, &dir, PORTABLE_MAX_EXTRACTED_BYTES)
                .context(t("解压便携包失败"))?;
            let root = plugin_host::bundle::unwrap_single_dir(&dir);
            validate_portable_root(&root)?;
            Some(root)
        }
    };
    Ok(Staged {
        kind: info.kind,
        version: info.latest.clone(),
        package,
        unpacked,
    })
}

/// 便携包解压后的根目录必须长得像一份程序。
///
/// 只放行白名单里的顶级项，并且要求三样关键东西都在：`rclone-gui.exe`、
/// `core/rclone.exe`、`licenses`。`core/rclone.exe` 是运行必需（正式包一直带着），
/// `licenses` 是分发合规要求。
fn validate_portable_root(root: &Path) -> Result<()> {
    if !root.is_dir() {
        bail!("{}", msg("便携包解压后没有目录：{}", &[&(root.display())]));
    }
    let mut has_exe = false;
    let mut has_core = false;
    let mut has_licenses = false;
    let entries =
        std::fs::read_dir(root).with_context(|| msg("读取目录 {} 失败", &[&(root.display())]))?;
    for e in entries {
        let e = e.context(t("读取便携包目录项失败"))?;
        let name = e.file_name().to_string_lossy().to_string();
        let meta = std::fs::symlink_metadata(e.path())
            .with_context(|| msg("读取 {} 失败", &[&(e.path().display())]))?;
        if meta.file_type().is_symlink() {
            // 链接可以指向包外（比如 C:\Windows），替换时就会把文件写到别处。
            bail!("{}", msg("便携包里有符号链接：{}", &[&(name)]));
        }
        if !PORTABLE_TOP.contains(&name.as_str()) {
            bail!(
                "{}",
                msg(
                    "便携包里有多余的顶级项 {}（只允许 {}）",
                    &[&(name), &(PORTABLE_TOP.join(", "))]
                )
            );
        }
        match name.as_str() {
            n if n == "rclone-gui.exe" => {
                if !meta.is_file() {
                    bail!("{}", t("便携包里的 rclone-gui.exe 不是文件"));
                }
                has_exe = true;
            }
            "core" => {
                if !meta.is_dir() {
                    bail!("{}", t("便携包里的 core 不是目录"));
                }
                let rclone = root.join("core").join("rclone.exe");
                let m = std::fs::symlink_metadata(&rclone).ok();
                match m {
                    Some(m) if m.is_file() => has_core = true,
                    _ => bail!("{}", t("便携包里缺少 core/rclone.exe")),
                }
            }
            "licenses" => has_licenses = true,
            _ => {}
        }
    }
    if !has_exe {
        bail!("{}", t("便携包里缺少 rclone-gui.exe"));
    }
    if !has_core {
        bail!("{}", t("便携包里缺少 core/rclone.exe"));
    }
    if !has_licenses {
        bail!("{}", t("便携包里缺少 licenses"));
    }
    reject_symlinks(root)?;
    Ok(())
}

/// 递归确认没有符号链接。Windows 上目录联接（junction）不报 symlink，
/// 但普通用户创建不了，且下面的路径包含检查会挡住跑出程序目录的写入。
fn reject_symlinks(dir: &Path) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| msg("读取 {} 失败", &[&(dir.display())]))? {
        let e = e.context(t("读取包目录失败"))?;
        let meta = std::fs::symlink_metadata(e.path())
            .with_context(|| msg("读取 {} 失败", &[&(e.path().display())]))?;
        if meta.file_type().is_symlink() {
            bail!("{}", msg("包里含有符号链接：{}", &[&(e.path().display())]));
        }
        if meta.is_dir() {
            reject_symlinks(&e.path())?;
        }
    }
    Ok(())
}

// ---------------- 替换计划 ----------------

/// 计划里的一个文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanFile {
    /// 目标绝对路径。
    dest: String,
    /// 新内容的绝对路径（在 staging 里）。
    src: String,
    /// 新内容的 sha256（小写）。
    sha256: String,
    /// 备份文件绝对路径（在临时目录里）。
    backup: String,
    /// 目标原先是否存在。不存在的话回滚就是删掉它。
    existed: bool,
}

/// 交给脚本执行的替换计划。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Plan {
    /// 要等的进程（本进程）。脚本等它退出后才动手。
    pid: u32,
    /// 该进程的启动时间（Windows FILETIME）。PID 会被系统复用，只比数字不够。
    #[serde(default)]
    start_ticks: u64,
    /// 主程序，替换完成后重新拉起它。
    exe: String,
    /// 程序目录。
    app_dir: String,
    /// 临时工作目录（备份、计划、脚本都在这里）。必须在程序目录之外。
    work_dir: String,
    /// 是否在替换后重启程序。
    restart: bool,
    files: Vec<PlanFile>,
}

/// 本进程的启动时间（FILETIME 计数，100ns）。取不到返回 0，脚本端会跳过这项校验。
#[cfg(windows)]
fn self_start_ticks() -> u64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    unsafe {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let ok = GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        );
        if ok.is_err() {
            return 0;
        }
        ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64
    }
}

#[cfg(not(windows))]
fn self_start_ticks() -> u64 {
    0
}

fn norm_path(s: &str) -> String {
    let s = s.replace('/', "\\");
    let s = s.trim_end_matches('\\').to_string();
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

/// `path` 是否在 `dir` 里面（含等于）。
fn inside(path: &Path, dir: &Path) -> bool {
    let p = norm_path(&path.to_string_lossy());
    let d = norm_path(&dir.to_string_lossy());
    p == d || p.starts_with(&format!("{d}\\"))
}

/// 唯一的工作目录：系统临时目录下的 `rcgui-update-<pid>-<序号>`。
fn work_dir() -> Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("rcgui-update-{}-{}", std::process::id(), stamp));
    std::fs::create_dir_all(&dir)
        .with_context(|| msg("创建临时目录 {} 失败", &[&(dir.display())]))?;
    Ok(dir)
}

/// 把便携版根目录下的所有文件排进计划。
///
/// 只收普通文件；`dest` 必须落在 `app_dir` 里、且不能落在 `data` 或备份目录下。
fn build_plan(root: &Path, app_dir: &Path, work: &Path, exe: &Path, restart: bool) -> Result<Plan> {
    if inside(work, app_dir) {
        // 备份目录在程序目录里就会被当成程序文件一起替换，甚至自己备份自己。
        bail!("{}", t("临时目录不能在程序目录里"));
    }
    if inside(root, app_dir) || inside(app_dir, root) {
        bail!("{}", t("新版本目录和程序目录不能互相包含"));
    }
    let mut files = Vec::new();
    collect_files(root, root, app_dir, work, &mut files)?;
    if files.is_empty() {
        bail!("{}", t("便携包里没有可替换的文件"));
    }
    Ok(Plan {
        pid: std::process::id(),
        start_ticks: self_start_ticks(),
        exe: exe.to_string_lossy().to_string(),
        app_dir: app_dir.to_string_lossy().to_string(),
        work_dir: work.to_string_lossy().to_string(),
        restart,
        files,
    })
}

fn collect_files(
    root: &Path,
    dir: &Path,
    app_dir: &Path,
    work: &Path,
    out: &mut Vec<PlanFile>,
) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| msg("读取 {} 失败", &[&(dir.display())]))? {
        let e = e.context(t("读取包目录失败"))?;
        let path = e.path();
        let meta = std::fs::symlink_metadata(&path)
            .with_context(|| msg("读取 {} 失败", &[&(path.display())]))?;
        if meta.file_type().is_symlink() {
            bail!("{}", msg("包里含有符号链接：{}", &[&(path.display())]));
        }
        if meta.is_dir() {
            collect_files(root, &path, app_dir, work, out)?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .with_context(|| t("计算相对路径失败"))?;
        let dest = app_dir.join(rel);
        // `data/` 是用户数据，配置文件和日志同理：白名单已经拦掉了，这里再兜一次。
        let first = rel
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if first == "data" {
            bail!(
                "{}",
                msg("新版本里含有用户数据目录：{}", &[&(rel.display())])
            );
        }
        if !inside(&dest, app_dir) || inside(&dest, &app_dir.join("data")) {
            bail!(
                "{}",
                msg("替换目标跑到程序目录之外：{}", &[&(dest.display())])
            );
        }
        let bytes =
            std::fs::read(&path).with_context(|| msg("读取 {} 失败", &[&(path.display())]))?;
        let backup = work.join("backup").join(rel);
        if inside(&backup, app_dir) {
            bail!("{}", t("备份目录不能放在程序目录里"));
        }
        out.push(PlanFile {
            dest: dest.to_string_lossy().to_string(),
            src: path.to_string_lossy().to_string(),
            sha256: sha256_hex(&bytes),
            backup: backup.to_string_lossy().to_string(),
            existed: dest.is_file(),
        });
    }
    Ok(())
}

/// 计划自检：路径都在该在的地方，目标不是用户数据。
fn validate_plan(plan: &Plan) -> Result<()> {
    if plan.files.is_empty() {
        bail!("{}", t("替换计划是空的"));
    }
    let app_dir = PathBuf::from(&plan.app_dir);
    let work = PathBuf::from(&plan.work_dir);
    if inside(&work, &app_dir) {
        bail!("{}", t("临时目录不能在程序目录里"));
    }
    for f in &plan.files {
        let dest = PathBuf::from(&f.dest);
        let backup = PathBuf::from(&f.backup);
        if !inside(&dest, &app_dir) {
            bail!("{}", msg("替换目标在程序目录之外：{}", &[&(f.dest)]));
        }
        if inside(&dest, &app_dir.join("data")) {
            bail!("{}", msg("不能替换用户数据：{}", &[&(f.dest)]));
        }
        if !inside(&backup, &work) {
            bail!("{}", msg("备份路径不在临时目录里：{}", &[&(f.backup)]));
        }
        if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("{}", msg("计划里的哈希不合法：{}", &[&(f.dest)]));
        }
    }
    Ok(())
}

// ---------------- 启动替换 ----------------

/// 把解压好的新版本整体复制到临时目录的 `payload/`，之后只从这份副本取文件。
///
/// 便携版解压目录通常就在 `<程序目录>/data/updates` 里，那是程序的下载目录；
/// 替换计划不允许"新版本目录和程序目录互相包含"（否则备份可能落进源树、
/// 复制可能自己套自己），所以必须先搬出去再规划。
fn copy_payload(root: &Path, work: &Path) -> Result<PathBuf> {
    let dst = work.join("payload");
    if dst.exists() {
        std::fs::remove_dir_all(&dst)
            .with_context(|| msg("清理旧的临时副本 {} 失败", &[&(dst.display())]))?;
    }
    std::fs::create_dir_all(&dst)
        .with_context(|| msg("创建临时目录 {} 失败", &[&(dst.display())]))?;
    let mut copied: u64 = 0;
    copy_tree(root, root, &dst, &mut copied)?;
    validate_portable_root(&dst)?;
    Ok(dst)
}

/// 逐文件复制并立即读回来比哈希。解压时校验的是"包里的字节"，这里要保证的是
/// "马上会被装上去的字节"，两件事分开算。
fn copy_tree(root: &Path, dir: &Path, dst_root: &Path, copied: &mut u64) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| msg("读取 {} 失败", &[&(dir.display())]))?
    {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)
            .with_context(|| msg("读取 {} 失败", &[&(path.display())]))?;
        if meta.file_type().is_symlink() {
            bail!("{}", msg("包里含有符号链接：{}", &[&(path.display())]));
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => bail!("{}", msg("复制时算不出相对路径：{}", &[&(path.display())])),
        };
        let target = dst_root.join(&rel);
        if meta.is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| msg("创建 {} 失败", &[&(target.display())]))?;
            copy_tree(root, &path, dst_root, copied)?;
            continue;
        }
        if !meta.is_file() {
            bail!(
                "{}",
                msg("便携包里有无法识别的项：{}", &[&(path.display())])
            );
        }
        let bytes = std::fs::read(&path)?;
        *copied += bytes.len() as u64;
        if *copied > PORTABLE_MAX_EXTRACTED_BYTES {
            bail!(
                "{}",
                msg(
                    "便携包解压后超过 {} 字节，已中止",
                    &[&PORTABLE_MAX_EXTRACTED_BYTES]
                )
            );
        }
        let expect = sha256_hex(&bytes);
        std::fs::write(&target, &bytes)
            .with_context(|| msg("写入 {} 失败", &[&(target.display())]))?;
        let back = std::fs::read(&target)?;
        if sha256_hex(&back) != expect {
            bail!(
                "{}",
                msg("复制到临时目录后校验失败：{}", &[&(target.display())])
            );
        }
    }
    Ok(())
}

/// 让脚本干活的 PowerShell 脚本。路径全部从 JSON 读，一律 `-LiteralPath`。
const APPLY_SCRIPT: &str = r#"param([Parameter(Mandatory=$true)][string]$PlanPath)
$ErrorActionPreference = 'Stop'

# 脚本是脱离宿主运行的，宿主退出后能说明结果的只剩退出码和这个日志文件。
$logPath = $null
try {
  $workDir = [System.IO.Path]::GetDirectoryName($PlanPath)
  $logPath = [System.IO.Path]::Combine($workDir, 'apply.log')
  Set-Content -LiteralPath $logPath -Value '' -Encoding utf8
} catch { $logPath = $null }

function Report([string]$msg) {
  Write-Output ('RCGUI_UPDATE ' + $msg)
  if ($null -ne $logPath) {
    try { Add-Content -LiteralPath $logPath -Value $msg -Encoding utf8 } catch { }
  }
}

try {
  $plan = Get-Content -LiteralPath $PlanPath -Raw -Encoding UTF8 | ConvertFrom-Json
} catch {
  Report 'ERROR plan'
  exit 10
}

# 1) 等旧进程退出。有界等待；启动时间用来确认等到的还是同一个进程
#    （PID 会被系统复用，光比数字可能等到一个完全无关的进程）。
$deadline = (Get-Date).AddSeconds(120)
while ($true) {
  $p = Get-Process -Id ([int]$plan.pid) -ErrorAction SilentlyContinue
  if (-not $p) { break }
  $ticks = 0
  try { $ticks = [int64]$p.StartTime.ToFileTimeUtc() } catch { break }
  if ([int64]$plan.start_ticks -ne 0 -and $ticks -ne [int64]$plan.start_ticks) { break }
  if ((Get-Date) -gt $deadline) {
    Report 'ERROR wait-timeout'
    exit 11
  }
  Start-Sleep -Milliseconds 200
}

$replaced = @()

# 还原一个文件，并用哈希证明还原的就是替换前那份内容。
# 还原本身也会失败（备份丢了、目标被占用），失败就 throw，由 Rollback 记账。
function RestoreOne($r) {
  $dest = [string]$r.dest
  if ($r.existed) {
    if (-not (Test-Path -LiteralPath ([string]$r.backup) -PathType Leaf)) { throw 'backup-missing' }
    Copy-Item -LiteralPath ([string]$r.backup) -Destination $dest -Force
    $now = (Get-FileHash -LiteralPath $dest -Algorithm SHA256).Hash
    if ($now -ne [string]$r.before) { throw 'restore-hash' }
  } else {
    if (Test-Path -LiteralPath $dest -PathType Leaf) { Remove-Item -LiteralPath $dest -Force }
    if (Test-Path -LiteralPath $dest -PathType Leaf) { throw 'restore-remove' }
  }
}

# 回滚按替换的相反顺序进行：后写的先撤，路径依赖才不会互相踩。
# 单个文件失败不中断其余文件的还原，失败的路径收集起来，由调用处如实报出。
$script:rollbackFailed = @()
function Rollback {
  for ($i = $replaced.Count - 1; $i -ge 0; $i--) {
    $r = $replaced[$i]
    try {
      RestoreOne $r
    } catch {
      $script:rollbackFailed += [string]$r.dest
      Report ('ERROR rollback-file ' + [string]$r.dest + ' (' + [string]$_.Exception.Message + ')')
    }
  }
}

# 2) 先备份全部文件并核对哈希。任何一份失败就原样退出，一个字节都不改。
#    $baseline 记下替换前的哈希，回滚时用它证明还原成功。
$baseline = @{}
foreach ($f in $plan.files) {
  $dest = [string]$f.dest
  try {
    if (Test-Path -LiteralPath $dest -PathType Leaf) {
      $before = (Get-FileHash -LiteralPath $dest -Algorithm SHA256).Hash
      # 不要用 `Split-Path -LiteralPath … -Parent`：这两个参数不在同一个参数集里，
      # PowerShell 会直接报"无法使用指定的命名参数解析参数集"。
      $dir = [System.IO.Path]::GetDirectoryName([string]$f.backup)
      if (-not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
      }
      Copy-Item -LiteralPath $dest -Destination ([string]$f.backup) -Force
      $after = (Get-FileHash -LiteralPath ([string]$f.backup) -Algorithm SHA256).Hash
      if ($before -ne $after) { throw 'backup-verify' }
      $baseline[$dest] = $before
    }
  } catch {
    Report ('ERROR backup ' + $dest + ' (' + [string]$_.Exception.Message + ')')
    exit 20
  }
}

# 3) 逐个替换。先写同目录临时文件并核对哈希，再改名覆盖。
foreach ($f in $plan.files) {
  $dest = [string]$f.dest
  $src = [string]$f.src
  $tmp = $dest + '.rcgui-new'
  $existed = Test-Path -LiteralPath $dest -PathType Leaf
  if (-not $existed) {
    $parent = [System.IO.Path]::GetDirectoryName($dest)
    if (-not (Test-Path -LiteralPath $parent)) {
      New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }
  }
  try {
    Copy-Item -LiteralPath $src -Destination $tmp -Force
    $h = (Get-FileHash -LiteralPath $tmp -Algorithm SHA256).Hash
    if ($h -ne [string]$f.sha256) { throw 'hash-mismatch' }
    Move-Item -LiteralPath $tmp -Destination $dest -Force
    $replaced += @{ dest = $dest; backup = [string]$f.backup; existed = $existed; before = [string]$baseline[$dest] }
  } catch {
    if (Test-Path -LiteralPath $tmp -PathType Leaf) { Remove-Item -LiteralPath $tmp -Force }
    Report ('ERROR replace ' + $dest + ' (' + [string]$_.Exception.Message + ')')
    Rollback
    if ($script:rollbackFailed.Count -gt 0) {
      # 有文件没能还原：不能说"已回滚"，只能如实报出是哪些，让人去手工核对。
      Report ('ERROR rollback ' + ($script:rollbackFailed -join ' '))
      exit 32
    }
    Report ('ROLLED-BACK ' + [string]$replaced.Count)
    exit 30
  }
}

# 4) 只在确实需要时重启自己。绝不启动、也绝不结束别的进程。
if ($plan.restart -eq $true -and (Test-Path -LiteralPath ([string]$plan.exe) -PathType Leaf)) {
  Start-Process -FilePath ([string]$plan.exe) -WorkingDirectory ([string]$plan.app_dir) | Out-Null
}
Report 'OK'
exit 0
"#;

/// 把计划和脚本写进工作目录，然后脱离本进程启动脚本。
fn spawn_helper(plan: &Plan, work: &Path) -> Result<PathBuf> {
    let plan_path = work.join("plan.json");
    let json = serde_json::to_vec_pretty(plan).context(t("生成替换计划失败"))?;
    std::fs::write(&plan_path, &json)
        .with_context(|| msg("写入 {} 失败", &[&(plan_path.display())]))?;
    let script = work.join("apply-update.ps1");
    std::fs::write(&script, APPLY_SCRIPT)
        .with_context(|| msg("写入 {} 失败", &[&(script.display())]))?;
    let mut child = None;
    for exe in ["pwsh.exe", "powershell.exe"] {
        match std::process::Command::new(exe)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .arg(&plan_path)
            .creation_flags(DETACHED_FLAGS)
            .spawn()
        {
            Ok(c) => {
                child = Some(c);
                break;
            }
            Err(_) => continue,
        }
    }
    let child = child.ok_or_else(|| anyhow::anyhow!("{}", t("找不到 PowerShell，无法完成替换")))?;
    // 脚本由系统接管，本进程随后退出；不 join，也不结束它。
    drop(child);
    Ok(plan_path)
}

#[cfg(windows)]
const DETACHED_FLAGS: u32 = 0x0000_0008 | 0x0000_0200 | 0x0800_0000; // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
#[cfg(not(windows))]
const DETACHED_FLAGS: u32 = 0;

trait CommandExt2 {
    fn creation_flags(&mut self, flags: u32) -> &mut Self;
}

#[cfg(windows)]
impl CommandExt2 for std::process::Command {
    fn creation_flags(&mut self, flags: u32) -> &mut Self {
        use std::os::windows::process::CommandExt;
        CommandExt::creation_flags(self, flags)
    }
}

#[cfg(not(windows))]
impl CommandExt2 for std::process::Command {
    fn creation_flags(&mut self, _flags: u32) -> &mut Self {
        self
    }
}

/// 启动已经校验过的安装器。
///
/// 安装器自己负责"下一步/取消"，取消就是关掉它的窗口，本程序这条路径到此为止。
/// 返回启动的文件路径，供界面提示"已启动安装程序"。
fn launch_setup(package: &Path) -> Result<PathBuf> {
    std::process::Command::new(package)
        .spawn()
        .with_context(|| msg("启动安装程序 {} 失败", &[&(package.display())]))?;
    Ok(package.to_path_buf())
}

/// 应用已下载的包。
///
/// - `Setup`：启动官方安装器（已校验哈希），返回安装器路径。
/// - `Portable`：生成替换计划和脚本，脱离本进程启动，返回计划文件路径。
///
/// 便携版替换在脚本里进行，所以本进程退出后才会真正覆盖 `rclone-gui.exe`；
/// 脚本不会结束任何它没有启动的进程，也不会碰 `data/`。
pub fn apply(staged: &Staged) -> Result<PathBuf> {
    match staged.kind {
        Kind::Setup => launch_setup(&staged.package),
        Kind::Portable => {
            let exe = std::env::current_exe().context(t("取不到本程序路径"))?;
            let app_dir = exe
                .parent()
                .ok_or_else(|| anyhow::anyhow!("{}", t("本程序没有所在目录")))?
                .to_path_buf();
            apply_at(staged, &exe, &app_dir)
        }
    }
}

/// `apply` 的实际动作，程序路径由调用方给出。
///
/// 单独拆出来是为了能对着临时目录测试：`apply` 用 `current_exe()`，测试里那样
/// 会指到 cargo 的 target 目录，可能把测试二进制自己换掉。
fn apply_at(staged: &Staged, exe: &Path, app_dir: &Path) -> Result<PathBuf> {
    let root = staged
        .unpacked
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("{}", t("便携包还没解压")))?;
    validate_portable_root(root)?;
    let (plan, work) = plan_for(root, exe, app_dir, true)?;
    spawn_helper(&plan, &work)
}

/// 生成替换计划：临时目录 → 外置副本 → 逐文件计划 → 自检。
///
/// 返回计划本身和临时目录（`spawn_helper` 还要往里写脚本）。
fn plan_for(root: &Path, exe: &Path, app_dir: &Path, restart: bool) -> Result<(Plan, PathBuf)> {
    let work = work_dir()?;
    let payload = copy_payload(root, &work)?;
    let plan = build_plan(&payload, app_dir, &work, exe, restart)?;
    validate_plan(&plan)?;
    Ok((plan, work))
}

/// 清掉 staging 目录。失败不报错：残留文件下次会被覆盖，不值得打断更新流程。
pub fn clean_staging(staging_dir: &Path) {
    let _ = std::fs::remove_dir_all(staging_dir);
}

// ---------------- 插件更新 ----------------

/// 已经装好的插件里，哪些可以从商店升级。返回值只含需要升的那些。
///
/// 升级走 [`crate::store::install`]，它内部保留 `kv.json`，因此插件攒下的令牌不会
/// 因为点了一次"全部更新"而消失。
pub fn plugin_update_targets(
    entries: &[crate::store::StoreEntry],
    installed: &[crate::store::Installed],
) -> Vec<crate::store::StoreEntry> {
    entries
        .iter()
        .filter(|e| {
            let now = installed
                .iter()
                .find(|i| i.id.eq_ignore_ascii_case(&e.id))
                .map(|i| i.version.as_str());
            crate::store::is_update(e, now)
        })
        .cloned()
        .collect()
}

/// 本进程运行时的真实 PID 字符串，给界面提示用。
pub fn pid_text() -> String {
    std::process::id().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p =
            std::env::temp_dir().join(format!("rcgui-upd-{tag}-{}-{stamp}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    /// 造一个合法的便携包根目录。
    fn portable_root(dir: &Path) {
        write(&dir.join("rclone-gui.exe"), b"NEW-EXE");
        write(&dir.join("core/rclone.exe"), b"NEW-CORE");
        write(&dir.join("licenses/COPYING"), b"GPL");
        write(&dir.join("assets/logo.png"), b"PNG");
        write(&dir.join("SHA256SUMS"), b"deadbeef");
    }

    // ---- 版本号 ----

    #[test]
    fn semver_compare_ignores_prefix() {
        assert!(is_newer("v0.3.0", "0.2.1"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.2.1", "0.2.1"));
        assert!(!is_newer("0.2.0", "0.2.1"));
        // 解析不出来就不提示更新
        assert!(!is_newer("latest", "0.2.1"));
        assert!(!is_newer("", "0.2.1"));
    }

    #[test]
    fn semver_orders_prerelease_correctly() {
        // 正式版高于同号预发布
        assert!(is_newer("1.0.0", "1.0.0-rc.1"));
        assert!(!is_newer("1.0.0-rc.1", "1.0.0"));
        // 预发布之间：数字段按数值比，数字低于字母
        assert!(is_newer("1.0.0-rc.2", "1.0.0-rc.1"));
        assert!(is_newer("1.0.0-rc.10", "1.0.0-rc.9"));
        assert!(is_newer("1.0.0-alpha", "1.0.0-1"));
        assert!(is_newer("1.0.0-rc.1", "1.0.0-beta.9"));
        // 前缀相同则更长的一方更大
        assert!(is_newer("1.0.0-rc.1.1", "1.0.0-rc.1"));
        // 构建元数据不参与比较
        assert!(!is_newer("1.0.0+build.9", "1.0.0"));
        assert!(!is_newer("1.0.0+build", "1.0.0+other"));
        // 预发布也要比主干：0.3.0-beta.1 > 0.2.1
        assert!(is_newer("0.3.0-beta.1", "0.2.1"));
    }

    #[test]
    fn semver_rejects_garbage() {
        assert!(parse_semver("1.2.3.4").is_none());
        assert!(parse_semver("1.x.0").is_none());
        assert!(parse_semver("1.0.0-").is_none());
        assert!(parse_semver("1.0.0-a..b").is_none());
        assert!(parse_semver("1.2").is_some());
        assert_eq!(parse_semver("1.2").unwrap().patch, 0);
    }

    // ---- 资产与地址 ----

    #[test]
    fn asset_name_is_exact() {
        assert_eq!(
            asset_name("0.3.0", Kind::Setup),
            "RCGUI-0.3.0-windows-x64-setup.exe"
        );
        assert_eq!(
            asset_name("v0.3.0", Kind::Portable),
            "RCGUI-0.3.0-windows-x64-portable.zip"
        );
    }

    #[test]
    fn sums_parse_both_formats() {
        let hash = "a".repeat(64);
        let sums = format!("{hash}  RCGUI-0.3.0-windows-x64-portable.zip\n{hash} *RCGUI-0.3.0-windows-x64-setup.exe\n");
        assert_eq!(
            sha_for_name(&sums, "RCGUI-0.3.0-windows-x64-portable.zip").as_deref(),
            Some(hash.as_str())
        );
        assert_eq!(
            sha_for_name(&sums, "RCGUI-0.3.0-windows-x64-setup.exe").as_deref(),
            Some(hash.as_str())
        );
        assert!(sha_for_name(&sums, "other.zip").is_none());
        // 短哈希或非十六进制都不算
        assert!(sha_for_name("abc  file.zip", "file.zip").is_none());
        assert!(sha_for_name(&format!("{}  file.zip", "z".repeat(64)), "file.zip").is_none());
    }

    #[test]
    fn only_github_hosts_allowed() {
        assert!(check_asset_url(
            "https://github.com/h897105167/rclone_gui_windows/releases/download/v0.3.0/x.exe"
        )
        .is_ok());
        assert!(check_asset_url("https://objects.githubusercontent.com/x").is_ok());
        assert!(check_asset_url("https://evil.example.com/x").is_err());
        assert!(check_asset_url("http://github.com/x").is_err());
        assert!(check_asset_url("https://github.com:8443/x").is_err());
        assert!(check_asset_url("https://user:pw@github.com/x").is_err());
        assert!(check_asset_url("https://github.com/x?token=1").is_err());
        assert!(check_asset_url("https://github.com/x#f").is_err());
        assert!(check_asset_url("not a url").is_err());
    }

    #[test]
    fn redirect_must_stay_on_github_https() {
        let u = |s: &str| reqwest::Url::parse(s).unwrap();
        // 官方下载会把 github.com 跳到这几个 CDN，这些要放行。
        assert!(redirect_ok(&u("https://github.com/x"), 0));
        assert!(redirect_ok(
            &u("https://objects.githubusercontent.com/x"),
            0
        ));
        assert!(redirect_ok(
            &u("https://release-assets.githubusercontent.com/x"),
            0
        ));
        // 明文降级、换域名、带用户名、非 443 端口、跳太多次，都不跟。
        assert!(!redirect_ok(&u("http://github.com/x"), 0));
        assert!(!redirect_ok(&u("https://evil.example.com/x"), 0));
        assert!(!redirect_ok(&u("https://user:pw@github.com/x"), 0));
        assert!(!redirect_ok(&u("https://github.com:8443/x"), 0));
        assert!(!redirect_ok(&u("https://github.com/x"), 5));
    }

    // ---- 下载前的自查：清单是别处拼出来的也不能放过 ----

    fn info_with(name: &str, url: &str, kind: Kind) -> UpdateInfo {
        UpdateInfo {
            current: "0.2.1".to_string(),
            latest: "0.3.0".to_string(),
            notes: String::new(),
            kind,
            asset: Asset {
                name: name.to_string(),
                url: url.to_string(),
                size: 1024,
            },
            sums_url: format!("https://github.com/{REPO}/releases/download/v0.3.0/{SUMS_NAME}"),
            sums_name: SUMS_NAME.to_string(),
        }
    }

    // stage 会先做这些本地检查再联网，所以这些用例一个字节都不会发出去。
    #[tokio::test]
    async fn stage_refuses_untrusted_manifest_before_network() {
        let dir = scratch("stage-guard");
        let good_url = format!(
            "https://github.com/{REPO}/releases/download/v0.3.0/{}",
            asset_name("0.3.0", Kind::Setup)
        );

        // 资产名和版本对不上
        let bad = info_with("RCGUI-0.2.9-windows-x64-setup.exe", &good_url, Kind::Setup);
        let err = stage(&bad, &dir).await.unwrap_err().to_string();
        assert!(err.contains("对不上"), "{err}");

        // 名字对但地址不在官方主机
        let name = asset_name("0.3.0", Kind::Setup);
        let bad = info_with(&name, "https://evil.example.com/pkg.exe", Kind::Setup);
        let err = stage(&bad, &dir).await.unwrap_err().to_string();
        assert!(err.contains("evil.example.com"), "{err}");

        // 清单地址带查询串（可能夹带令牌）
        let mut bad = info_with(&name, &good_url, Kind::Setup);
        bad.sums_url = format!("{good_url}?token=x");
        let err = stage(&bad, &dir).await.unwrap_err().to_string();
        assert!(err.contains("查询串"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn release_url_must_bind_repo_tag_and_file() {
        let name = "RCGUI-0.3.0-windows-x64-setup.exe";
        let good = format!("https://github.com/{REPO}/releases/download/v0.3.0/{name}");
        assert!(check_release_asset_url(&good, "v0.3.0", name).is_ok());
        // 别的仓库
        assert!(check_release_asset_url(
            &format!("https://github.com/evil/repo/releases/download/v0.3.0/{name}"),
            "v0.3.0",
            name
        )
        .is_err());
        // 别的标签
        assert!(check_release_asset_url(&good, "v0.4.0", name).is_err());
        // 别的文件名
        assert!(
            check_release_asset_url(&good, "v0.3.0", "RCGUI-0.4.0-windows-x64-setup.exe").is_err()
        );
    }

    // ---- 发布清单 ----

    fn release_json(tag: &str, size: u64) -> String {
        let setup = asset_name(tag.trim_start_matches('v'), Kind::Setup);
        let portable = asset_name(tag.trim_start_matches('v'), Kind::Portable);
        let base = format!("https://github.com/{REPO}/releases/download/{tag}");
        format!(
            r#"{{"tag_name":"{tag}","body":"notes here","assets":[
              {{"name":"{setup}","browser_download_url":"{base}/{setup}","size":{size}}},
              {{"name":"{portable}","browser_download_url":"{base}/{portable}","size":123}},
              {{"name":"SHA256SUMS","browser_download_url":"{base}/SHA256SUMS","size":99}}
            ]}}"#
        )
    }

    #[test]
    fn parse_release_picks_matching_asset_and_sums() {
        let info = parse_release(&release_json("v0.3.0", 10), "0.2.1", Kind::Setup).unwrap();
        assert_eq!(info.latest, "0.3.0");
        assert_eq!(info.asset.name, "RCGUI-0.3.0-windows-x64-setup.exe");
        assert_eq!(
            info.sums_url,
            format!("https://github.com/{REPO}/releases/download/v0.3.0/SHA256SUMS")
        );
        assert_eq!(info.notes, "notes here");
    }

    #[test]
    fn parse_release_refuses_wrong_asset_and_same_version() {
        // 版本没变
        assert!(parse_release(&release_json("v0.2.1", 10), "0.2.1", Kind::Setup).is_err());
        // 只有 setup，要 portable
        let only_setup = format!(
            r#"{{"tag_name":"v0.3.0","assets":[{{"name":"RCGUI-0.3.0-windows-x64-setup.exe","browser_download_url":"https://github.com/{REPO}/releases/download/v0.3.0/RCGUI-0.3.0-windows-x64-setup.exe","size":1}},{{"name":"SHA256SUMS","browser_download_url":"https://github.com/{REPO}/releases/download/v0.3.0/SHA256SUMS","size":1}}]}}"#
        );
        assert!(parse_release(&only_setup, "0.2.1", Kind::Portable).is_err());
        // 资产地址指向别的仓库
        let wrong_repo = format!(
            r#"{{"tag_name":"v0.3.0","assets":[{{"name":"RCGUI-0.3.0-windows-x64-setup.exe","browser_download_url":"https://github.com/evil/repo/releases/download/v0.3.0/RCGUI-0.3.0-windows-x64-setup.exe","size":1}},{{"name":"SHA256SUMS","browser_download_url":"https://github.com/evil/repo/releases/download/v0.3.0/SHA256SUMS","size":1}}]}}"#
        );
        assert!(parse_release(&wrong_repo, "0.2.1", Kind::Setup).is_err());
        // 超过大小上限
        assert!(parse_release(
            &release_json("v0.3.0", MAX_ASSET_BYTES + 1),
            "0.2.1",
            Kind::Setup
        )
        .is_err());
    }

    #[test]
    fn parse_release_surfaces_api_error_message() {
        let err = parse_release(
            r#"{"message":"API rate limit exceeded"}"#,
            "0.2.1",
            Kind::Setup,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("API rate limit exceeded"), "{err}");
    }

    // ---- 便携包结构校验 ----

    #[test]
    fn portable_root_accepts_whitelist() {
        let dir = scratch("root-ok");
        portable_root(&dir);
        assert!(validate_portable_root(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn portable_root_rejects_missing_exe() {
        // 只缺新 exe 的包必须被拒 —— 旧版曾经因为检查的是旧 exe 而放行。
        let dir = scratch("root-noexe");
        portable_root(&dir);
        std::fs::remove_file(dir.join("rclone-gui.exe")).unwrap();
        let err = validate_portable_root(&dir).unwrap_err().to_string();
        assert!(err.contains("rclone-gui.exe"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn portable_root_rejects_missing_core_and_licenses() {
        let dir = scratch("root-nocore");
        portable_root(&dir);
        std::fs::remove_file(dir.join("core/rclone.exe")).unwrap();
        assert!(validate_portable_root(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);

        let dir = scratch("root-nolic");
        portable_root(&dir);
        std::fs::remove_dir_all(dir.join("licenses")).unwrap();
        assert!(validate_portable_root(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn portable_root_rejects_data_and_extra_entries() {
        let dir = scratch("root-data");
        portable_root(&dir);
        write(&dir.join("data/rcgui.conf"), b"{\"secret\":1}");
        let err = validate_portable_root(&dir).unwrap_err().to_string();
        assert!(err.contains("data"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);

        let dir = scratch("root-extra");
        portable_root(&dir);
        write(&dir.join("installer.ps1"), b"evil");
        assert!(validate_portable_root(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn portable_root_rejects_symlink() {
        let dir = scratch("root-link");
        portable_root(&dir);
        // 链接目标要放在 root 外面：root 里多出一个文件会先触发"多余顶级项"，
        // 那样测到的是白名单而不是链接规则。
        let outside = scratch("root-link-target");
        let target = outside.join("real.txt");
        write(&target, b"x");
        let link = dir.join("assets").join("link.txt");
        // 普通用户可能没有创建符号链接的权限，那就不测这一条。
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_dir_all(&outside);
            return;
        }
        let err = validate_portable_root(&dir).unwrap_err().to_string();
        assert!(err.contains("链接"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    // ---- 替换计划：真实文件系统 ----

    struct Env {
        app: PathBuf,
        root: PathBuf,
        work: PathBuf,
    }

    /// 造一个程序目录（旧 exe + core + 用户数据）和一份新版本目录。
    fn env(tag: &str) -> Env {
        let base = scratch(tag);
        let app = base.join("app");
        let root = base.join("new");
        let work = base.join("work");
        write(&app.join("rclone-gui.exe"), b"OLD-EXE");
        write(&app.join("core/rclone.exe"), b"OLD-CORE");
        write(&app.join("data/sentinel.txt"), b"USER-DATA");
        portable_root(&root);
        std::fs::create_dir_all(&work).unwrap();
        Env { app, root, work }
    }

    fn exe(e: &Env) -> PathBuf {
        e.app.join("rclone-gui.exe")
    }

    // ---- 替换计划 → 真正发货的 PowerShell 脚本 ----

    /// 跑真正发货的那段脚本（就是 `spawn_helper` 写出去的那份原文）。
    ///
    /// 计划里的 PID 指向本进程、启动时间故意写成 1：脚本的有界等待第一次比较
    /// 就会放行，不必等测试进程自己退出（否则要白等 120 秒）。
    fn run_script(plan: &Plan, work: &Path, shell: &'static str) -> std::process::Output {
        let mut plan = plan.clone();
        plan.pid = std::process::id();
        plan.start_ticks = 1;
        let plan_path = work.join("plan.json");
        std::fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
        let script = work.join("apply-update.ps1");
        std::fs::write(&script, APPLY_SCRIPT).unwrap();
        std::process::Command::new(shell)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .arg(&plan_path)
            .output()
            .unwrap()
    }

    fn both_streams(out: &std::process::Output) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    #[test]
    fn script_replaces_program_and_keeps_user_data() {
        let Some(shell) = powershell() else {
            return;
        };
        let e = env("ok");
        let plan = build_plan(&e.root, &e.app, &e.work, &exe(&e), false).unwrap();
        assert!(plan.files.iter().all(|f| !f.dest.contains("sentinel")));

        let started = std::time::Instant::now();
        let out = run_script(&plan, &e.work, shell);
        let text = both_streams(&out);
        assert!(out.status.success(), "{text}");
        assert!(text.contains("RCGUI_UPDATE OK"), "{text}");
        // 认不出 PID 复用就会白等 120 秒
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "{text}"
        );
        assert_eq!(std::fs::read(exe(&e)).unwrap(), b"NEW-EXE");
        assert_eq!(
            std::fs::read(e.app.join("core/rclone.exe")).unwrap(),
            b"NEW-CORE"
        );
        assert_eq!(
            std::fs::read(e.app.join("licenses/COPYING")).unwrap(),
            b"GPL"
        );
        // 用户数据原封不动
        assert_eq!(
            std::fs::read(e.app.join("data/sentinel.txt")).unwrap(),
            b"USER-DATA"
        );
        // 备份在临时目录里、不在程序目录里，而且内容就是旧文件
        assert!(!inside(&e.work, &e.app));
        for f in &plan.files {
            if f.existed {
                assert!(Path::new(&f.backup).is_file(), "{}", f.backup);
            }
        }
        assert_eq!(
            std::fs::read(e.work.join("backup/rclone-gui.exe")).unwrap(),
            b"OLD-EXE"
        );
        // 临时文件不留在程序目录里
        assert!(!e.app.join("rclone-gui.exe.rcgui-new").exists());
        // 脚本脱离运行，宿主退出后只剩退出码和这份日志
        let log = std::fs::read_to_string(e.work.join("apply.log")).unwrap();
        assert!(log.contains("OK"), "{log}");
        let _ = std::fs::remove_dir_all(e.app.parent().unwrap());
    }

    #[test]
    fn plan_refuses_work_dir_inside_app_dir() {
        // 旧实现把备份放在 portable 的 staging 里（程序目录内部），robocopy 会自己备份自己。
        let e = env("selfrec");
        let bad = e.app.join("data/updates/old");
        std::fs::create_dir_all(&bad).unwrap();
        let err = build_plan(&e.root, &e.app, &bad, &exe(&e), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("临时目录"), "{err}");
        let _ = std::fs::remove_dir_all(e.app.parent().unwrap());
    }

    #[test]
    fn plan_refuses_replace_outside_app_dir() {
        let e = env("outsider");
        let mut plan = build_plan(&e.root, &e.app, &e.work, &exe(&e), false).unwrap();
        plan.files[0].dest = e
            .app
            .parent()
            .unwrap()
            .join("evil.exe")
            .to_string_lossy()
            .to_string();
        assert!(validate_plan(&plan).is_err());
        let _ = std::fs::remove_dir_all(e.app.parent().unwrap());
    }

    #[test]
    fn plan_refuses_replace_into_user_data() {
        let e = env("intodata");
        let mut plan = build_plan(&e.root, &e.app, &e.work, &exe(&e), false).unwrap();
        plan.files[0].dest = e
            .app
            .join("data/sentinel.txt")
            .to_string_lossy()
            .to_string();
        assert!(validate_plan(&plan).is_err());
        let _ = std::fs::remove_dir_all(e.app.parent().unwrap());
    }

    #[test]
    fn plan_for_moves_payload_out_of_data_staging() {
        // main 的真实布局：解压目录就在 <程序目录>/data/updates 里。
        let e = env("inlayouts");
        let staging = e.app.join("data/updates");
        let root = staging.join("unpacked/RCGUI-0.3.0-portable");
        portable_root(&root);
        write(&staging.join("RCGUI-0.3.0-portable.zip"), b"ZIP");

        // 直接拿它规划一定被拒，所以 apply 才要先复制出去。
        let work0 = e.app.parent().unwrap().join("work0");
        std::fs::create_dir_all(&work0).unwrap();
        let err = build_plan(&root, &e.app, &work0, &exe(&e), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("互相包含"), "{err}");

        let (plan, work) = plan_for(&root, &exe(&e), &e.app, false).unwrap();
        // 副本和备份都在程序目录之外
        assert!(
            !inside(&work, &e.app),
            "临时目录在程序目录里：{}",
            work.display()
        );
        assert!(
            !inside(&e.app, &work),
            "程序目录在临时目录里：{}",
            work.display()
        );
        let payload = work.join("payload");
        assert!(payload.is_dir());
        for f in &plan.files {
            assert!(
                Path::new(&f.src).starts_with(&payload),
                "{} 应来自副本",
                f.src
            );
            assert!(
                Path::new(&f.backup).starts_with(&work),
                "{} 应在临时目录里",
                f.backup
            );
            assert!(
                inside(Path::new(&f.dest), &e.app),
                "{} 应在程序目录里",
                f.dest
            );
        }

        if let Some(shell) = powershell() {
            let out = run_script(&plan, &work, shell);
            let text = both_streams(&out);
            assert!(out.status.success(), "{text}");
            assert_eq!(std::fs::read(exe(&e)).unwrap(), b"NEW-EXE");
            assert_eq!(
                std::fs::read(e.app.join("core/rclone.exe")).unwrap(),
                b"NEW-CORE"
            );
            assert_eq!(
                std::fs::read(e.app.join("data/sentinel.txt")).unwrap(),
                b"USER-DATA"
            );
            // 原来的下载和解压目录没被删、也没被当成备份源
            assert!(root.join("rclone-gui.exe").is_file());
            assert!(staging.join("RCGUI-0.3.0-portable.zip").is_file());
        }
        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(e.app.parent().unwrap());
    }

    #[test]
    fn script_refuses_file_whose_hash_changed() {
        let Some(shell) = powershell() else {
            return;
        };
        let e = env("badhash");
        let mut plan = build_plan(&e.root, &e.app, &e.work, &exe(&e), false).unwrap();
        // 计划里的哈希和 staging 里的文件对不上（比如下载后被替换过）
        plan.files[0].sha256 = "b".repeat(64);

        let out = run_script(&plan, &e.work, shell);
        let text = both_streams(&out);
        assert_eq!(out.status.code(), Some(30), "{text}");
        assert!(text.contains("hash-mismatch"), "{text}");
        assert_eq!(std::fs::read(exe(&e)).unwrap(), b"OLD-EXE");
        assert!(!e.app.join("rclone-gui.exe.rcgui-new").exists());
        let _ = std::fs::remove_dir_all(e.app.parent().unwrap());
    }

    #[test]
    fn script_rolls_back_when_source_disappears_midway() {
        let Some(shell) = powershell() else {
            return;
        };
        let e = env("rollback");
        let mut plan = build_plan(&e.root, &e.app, &e.work, &exe(&e), false).unwrap();
        // 让最后一个文件读不到：前面的替换必须被还原。
        let last = plan.files.len() - 1;
        plan.files[last].src = e.root.join("missing.bin").to_string_lossy().to_string();
        assert!(last > 0, "至少要替换过两个文件才能测中途失败");

        let out = run_script(&plan, &e.work, shell);
        let text = both_streams(&out);
        assert_eq!(out.status.code(), Some(30), "{text}");
        assert!(text.contains("ROLLED-BACK"), "{text}");
        assert_eq!(
            std::fs::read(exe(&e)).unwrap(),
            b"OLD-EXE",
            "第一个文件必须回滚"
        );
        assert_eq!(
            std::fs::read(e.app.join("core/rclone.exe")).unwrap(),
            b"OLD-CORE"
        );
        assert_eq!(
            std::fs::read(e.app.join("data/sentinel.txt")).unwrap(),
            b"USER-DATA"
        );
        assert!(!e.app.join("rclone-gui.exe.rcgui-new").exists());
        let _ = std::fs::remove_dir_all(e.app.parent().unwrap());
    }

    #[test]
    fn script_removes_new_file_on_rollback() {
        let Some(shell) = powershell() else {
            return;
        };
        let e = env("newfile");
        write(&e.root.join("plugins/extra.dll"), b"EXTRA");
        let mut plan = build_plan(&e.root, &e.app, &e.work, &exe(&e), false).unwrap();
        let idx = plan
            .files
            .iter()
            .position(|f| f.dest.ends_with("extra.dll"))
            .unwrap();
        assert!(!plan.files[idx].existed);
        let last = plan.files.len() - 1;
        plan.files[last].src = e.root.join("missing.bin").to_string_lossy().to_string();

        let out = run_script(&plan, &e.work, shell);
        assert_eq!(out.status.code(), Some(30), "{}", both_streams(&out));
        assert!(
            !e.app.join("plugins/extra.dll").exists(),
            "新建的文件必须被删掉"
        );
        let _ = std::fs::remove_dir_all(e.app.parent().unwrap());
    }

    #[test]
    fn script_does_not_restart_a_missing_exe() {
        let Some(shell) = powershell() else {
            return;
        };
        let e = env("norestart");
        let mut plan = build_plan(&e.root, &e.app, &e.work, &exe(&e), false).unwrap();
        plan.restart = true;
        plan.exe = e.app.join("不存在的.exe").to_string_lossy().to_string();

        let out = run_script(&plan, &e.work, shell);
        let text = both_streams(&out);
        // 程序都不在了就别硬拉，替换本身算成功
        assert!(out.status.success(), "{text}");
        assert!(text.contains("RCGUI_UPDATE OK"), "{text}");
        let _ = std::fs::remove_dir_all(e.app.parent().unwrap());
    }

    #[test]
    fn json_plan_round_trips_and_has_no_shell_quoting() {
        let base = scratch("json");
        // 目录名带空格、中文、百分号、单引号 —— 旧实现把这些拼进 .cmd 里，
        // % 会被二次展开，' 和空格会拆开参数。
        let app = base.join("程序 目录 %TEMP% 'x'");
        let root = base.join("新的 版本 %1%");
        let work = base.join("临时 %PATH% 'q'");
        write(&app.join("rclone-gui.exe"), b"OLD");
        write(&app.join("data/sentinel.txt"), b"USER");
        write(&root.join("rclone-gui.exe"), b"NEW");
        write(&root.join("core/rclone.exe"), b"NEWCORE");
        write(&root.join("licenses/COPYING"), b"GPL");
        std::fs::create_dir_all(&work).unwrap();
        let plan = build_plan(&root, &app, &work, &app.join("rclone-gui.exe"), true).unwrap();
        let json = serde_json::to_string(&plan).unwrap();
        // JSON 里路径原样保留（百分号不展开），解析回来完全一致
        assert!(json.contains("%TEMP%"));
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.files.len(), plan.files.len());
        assert_eq!(back.files[0].dest, plan.files[0].dest);
        assert!(back.restart);
        // 测试里不真的拉起程序：那个 exe 只是几个字节的假文件。
        let mut run = back.clone();
        run.restart = false;
        if let Some(shell) = powershell() {
            let out = run_script(&run, &work, shell);
            let text = both_streams(&out);
            assert!(out.status.success(), "{text}");
            assert_eq!(std::fs::read(app.join("rclone-gui.exe")).unwrap(), b"NEW");
            assert_eq!(
                std::fs::read(app.join("data/sentinel.txt")).unwrap(),
                b"USER"
            );
        }
        // 脚本用 -LiteralPath 读路径，从不把路径拼进命令字符串
        assert!(APPLY_SCRIPT.contains("-LiteralPath"));
        assert!(!APPLY_SCRIPT.contains("cmd /c"));
        assert!(!APPLY_SCRIPT.contains("robocopy"));
        assert!(!APPLY_SCRIPT.contains("Stop-Process"));
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- 其它 ----

    #[test]
    fn reentry_is_refused_until_guard_drops() {
        let g = try_acquire();
        assert!(g.is_some());
        assert!(try_acquire().is_none());
        drop(g);
        assert!(try_acquire().is_some());
    }

    /// 找一个能跑脚本的 PowerShell。找不到就跳过：替换只由这段脚本执行，
    /// 没有脚本就没有可验证的实现，跳过时要能从测试名看出来。
    fn powershell() -> Option<&'static str> {
        for exe in ["pwsh", "powershell"] {
            let ok = std::process::Command::new(exe)
                .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                return Some(exe);
            }
        }
        None
    }

    #[test]
    fn plugin_update_targets_only_newer() {
        use crate::store::{Installed, StoreEntry};
        let entry = |id: &str, ver: &str| StoreEntry {
            id: id.into(),
            name: id.into(),
            version: ver.into(),
            description: String::new(),
            repository: String::new(),
            url: "https://github.com/o/r/x.zip".into(),
            sha256: "a".repeat(64),
            min_gui_version: String::new(),
            capabilities: vec![],
        };
        let entries = vec![
            entry("kv", "1.1.0"),
            entry("new", "1.0.0"),
            entry("old", "1.0.0"),
        ];
        let installed = vec![
            Installed {
                id: "kv".into(),
                version: "1.0.0".into(),
            },
            Installed {
                id: "old".into(),
                version: "2.0.0".into(),
            },
        ];
        let targets = plugin_update_targets(&entries, &installed);
        let ids: Vec<&str> = targets.iter().map(|e| e.id.as_str()).collect();
        // 只升级已装且商店版本更新的：`old` 已装 2.0.0 比商店的 1.0.0 新，不动；
        // `new` 根本没装，那是"安装"不是"升级"，同样不出现在这里。
        assert_eq!(ids, vec!["kv"]);
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
