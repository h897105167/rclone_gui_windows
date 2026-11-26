//! 把 `rcgui.conf` 推到 / 拉自 WebDAV（或任何支持 PUT/GET 的 HTTPS 端点）。
//!
//! 四条硬规则：
//!
//! 1. **只走 HTTPS**。配置里含远程凭据（可能含 OAuth token），明文传输等于泄漏。
//!    唯一例外是本机回环地址 —— 开发时起个本地 WebDAV 调试不算错，但别的
//!    `http://` 一律拒绝。地址里也不许写 `user:pass@`，凭据只能走认证头。
//! 2. **拉取不覆盖**。下载回来的字节只作为原始数据返回，调用方先给用户看预览，
//!    确认了才写入。点了"拉取"最坏的结果是多看一个对话框。
//! 3. **同一时刻只跑一次**。推送/拉取都会读写用户的远端配置，两个并发请求互相
//!    盖掉的话没人能说清最后留下的是哪份。用 [`Session`] 把整段流程（请求 +
//!    本地落盘）都圈在同一个闸门里。
//! 4. **不跟随重定向**。跟随会让 HTTPS 被降级成 HTTP，也会把 Basic 凭据送到
//!    另一个域。遇到 3xx 直接报错，让用户改地址。
//!
//! 密码只出现在 Basic 认证头里，不写日志、不进错误信息。

use anyhow::{anyhow, bail, Context as _, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 单个配置文件的大小上限。rcgui.conf 是纯元数据，4 MiB 已经非常宽松；
/// 设上限是为了不让一个坏端点把内存吃光 —— 分块响应没有 Content-Length，
/// 只能边读边数。
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// 整次请求的上限时间。
const TIMEOUT: Duration = Duration::from_secs(60);
/// 建立连接的上限时间，比整次请求短，免得地址不可达时干等。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 一次同步的闸门。`true` 表示已经有一次在跑。
///
/// 用原子布尔而不是 `MutexGuard`：票据要跟着整个同步流程一起跨 `.await`，
/// 持有锁的 guard 不是 `Send`，`tokio::spawn` 起来的任务就编不过（Future 不能
/// 跨线程）。原子布尔没有这个问题，而且这里只需要"有人占着"这一个信息。
static GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 占住闸门。返回值必须活到同步结束 —— 它 drop 的那一刻才真正放行。
///
/// `Send + Sync` 是硬要求：调用方会把它塞进 `tokio::spawn` 的任务里。
pub struct Ticket(());

impl Drop for Ticket {
    fn drop(&mut self) {
        GATE.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 尝试占住闸门；已有同步在跑时返回 `None`。
pub fn try_acquire() -> Option<Ticket> {
    GATE.compare_exchange(
        false,
        true,
        std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst,
    )
    .ok()
    .map(|_| Ticket(()))
}

/// 占住闸门并保管到本次同步全部结束（请求 + 落盘）。
///
/// 推荐调用方整段都用它：`Session::pull` 拿到字节、给用户预览、`Session::save`
/// 落盘，中间不会有第二个请求插进来。如果只用下面的自由函数 [`pull`]/[`push`]，
/// 就得自己持有 [`Ticket`]，否则两个请求可能互相覆盖。
pub struct Session {
    _ticket: Ticket,
}

impl Session {
    /// 开始一次同步；已有同步在跑时返回 Err（而不是排队，避免用户以为没反应）。
    pub fn begin() -> Result<Session> {
        Ok(Session {
            _ticket: take_ticket()?,
        })
    }

    /// 下载远端配置，返回原始字节。只返回字节，不写文件、不应用。
    ///
    /// 走内部实现而不是自由函数：闸门已经由本 `Session` 占着，再去抢一次
    /// 只会自己挡住自己。
    pub async fn pull(&self, url: &str, user: &str, pass: &str) -> Result<Vec<u8>> {
        pull_inner(url, user, pass, TIMEOUT).await
    }

    /// 上传配置（版本导出的内容）。
    pub async fn push(&self, url: &str, user: &str, pass: &str, body: Vec<u8>) -> Result<()> {
        push_inner(url, user, pass, body).await
    }

    /// 落盘一份本地副本。写失败就返回 Err —— 不允许"报成功但其实没写"。
    pub fn save(&self, data_dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        save_local_copy(data_dir, name, bytes)
    }
}

/// 编译期证据：票据和会话可以跨线程搬。
///
/// main 会把同步流程 `tokio::spawn` 出去，只要闸门类型不是 `Send`，那边的
/// `Future` 就不是 `Send`，编译不过。把这个错误提前到这里，一眼能看出原因。
const _: fn() = || {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<Ticket>();
    assert_sync::<Ticket>();
    assert_send::<Session>();
    assert_sync::<Session>();
};

/// 只接受 HTTPS；本机回环允许 HTTP。地址里不许带 userinfo。
pub fn check_url(url: &str) -> Result<()> {
    let u = url.trim();
    if u.is_empty() {
        bail!("{}", rcgui_i18n::t("还没填备份地址"));
    }
    let lower = u.to_ascii_lowercase();
    let https = lower.starts_with("https://") && u.len() > 8;
    let loopback = match lower.strip_prefix("http://") {
        Some(rest) => {
            let host = authority(rest);
            host == "127.0.0.1" || host == "localhost" || host == "[::1]"
        }
        None => false,
    };
    if !https && !loopback {
        // 报错时用脱敏后的地址：用户填错的地址里可能带着 `user:pass@` 或 `?token=`
        // 之类的凭据，原文抄进错误信息就会出现在界面、日志和截图里。
        if lower.starts_with("http://") {
            bail!(
                "{}",
                rcgui_i18n::f(
                    "只接受 HTTPS 地址：{} 是本机之外的明文 HTTP，传输配置会泄漏凭据",
                    &[&(strip_userinfo(u))]
                )
            );
        }
        bail!(
            "{}",
            rcgui_i18n::f(
                "只接受 https:// 开头的地址（当前：{}）",
                &[&(strip_userinfo(u))]
            )
        );
    }
    if has_userinfo(u) {
        bail!(
            "{}",
            rcgui_i18n::f(
                "地址里不要写用户名密码（{}），请填到下面的用户名/密码栏，否则会随 URL 出现在日志里",
                &[&(strip_userinfo(u))]
            )
        );
    }
    Ok(())
}

/// 地址的第一段（authority）里是否含 `@`。
fn has_userinfo(url: &str) -> bool {
    let Some(scheme_end) = url.find("://") else {
        return false;
    };
    let rest = &url[scheme_end + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    rest[..end].contains('@')
}

/// 从 "scheme://" 之后的部分取出主机名：去掉端口，保留 IPv6 的方括号。
fn authority(rest: &str) -> &str {
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    if host.starts_with('[') {
        // `[::1]:8080` -> `[::1]`
        match host.find(']') {
            Some(i) => &host[..=i],
            None => host,
        }
    } else {
        host.split(':').next().unwrap_or("")
    }
}

/// 从地址推出本地缓存文件名。
///
/// 只取路径的最后一段并过滤成安全字符：远端地址由用户填，不能让它决定本地路径
/// （`../..` 之类会跑到数据目录之外）。主机名不算路径，`https://e.com/` 没有文件名。
pub fn local_name_for(url: &str) -> String {
    let trimmed = url.trim();
    let after_scheme = match trimmed.find("://") {
        Some(i) => &trimmed[i + 3..],
        None => trimmed,
    };
    let path = match after_scheme.find(['/', '?', '#']) {
        Some(i) => &after_scheme[i..],
        None => "",
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let safe: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_matches('.').to_string();
    if safe.is_empty() {
        "rcgui.conf".to_string()
    } else {
        safe
    }
}

/// 本地存档目录（`<数据目录>/backups`）。
pub fn local_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("backups")
}

/// 校验用户给的本地文件名：只接受纯文件名。
///
/// 名字来自地址栏和配置，不能让它决定路径。带分隔符、`..`、盘符或绝对路径的
/// 一律报错，而不是悄悄"修正"成别的名字 —— 静默改名会让用户找不到自己的文件，
/// 而且看起来像是成功写到了别处。
fn safe_file_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        bail!("{}", rcgui_i18n::t("本地副本的文件名为空"));
    }
    if n.contains('/') || n.contains('\\') {
        bail!(
            "{}",
            rcgui_i18n::f("本地副本的文件名不能带路径分隔符：{}", &[&(n)])
        );
    }
    if n == "." || n == ".." {
        bail!(
            "{}",
            rcgui_i18n::f("本地副本的文件名不能用 {} 这样的名字", &[&(n)])
        );
    }
    if n.contains(':') || Path::new(n).is_absolute() {
        bail!(
            "{}",
            rcgui_i18n::f("本地副本的文件名不能是绝对路径或带盘符：{}", &[&(n)])
        );
    }
    Ok(local_name_for(n))
}

/// 保存一份本地副本，返回路径。
///
/// 推送前先落盘：远端失败时用户手里还有文件，不用重新导出一次。
/// 写完会核对字节数，磁盘满之类的问题会变成 Err 而不是悄悄留下半个文件。
pub fn save_local_copy(data_dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let safe = safe_file_name(name)?;
    let dir = local_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| rcgui_i18n::f("创建目录 {} 失败", &[&(dir.display())]))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("{stamp}-{safe}"));
    // 上面两道检查之后这里不该发生，但落盘前再确认一次：写出去的文件必须在
    // backups 目录里，宁可报错也不要把用户数据写到别处。
    if !path.starts_with(&dir) {
        bail!(
            "{}",
            rcgui_i18n::f("拒绝写到备份目录之外：{}", &[&(path.display())])
        );
    }
    std::fs::write(&path, bytes)
        .with_context(|| rcgui_i18n::f("写入 {} 失败", &[&(path.display())]))?;
    let written = std::fs::metadata(&path)
        .with_context(|| rcgui_i18n::f("读取 {} 失败", &[&(path.display())]))?
        .len();
    if written != bytes.len() as u64 {
        bail!(
            "{}",
            rcgui_i18n::f(
                "写入 {} 不完整（{} / {} 字节）",
                &[&(path.display()), &(written), &(bytes.len())]
            )
        );
    }
    Ok(path)
}

/// HTTP 客户端：不跟随重定向，整次请求有超时，连接也有超时。
fn client_with(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT)
        // 跟随重定向会同时降级协议、把认证头送到别的域，这里一律拒绝。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context(rcgui_i18n::t("创建 HTTP 客户端失败"))
}

fn client() -> Result<reqwest::Client> {
    client_with(TIMEOUT)
}

/// GET 一份配置。返回字节（已限流）。
///
/// 不写文件、不应用，只把远端内容交给调用方；调用方应先给用户预览。
/// 自己占闸门（整次请求期间），所以只要一个请求走这里就够了；要连着预览和
/// 落盘一起圈住，用 [`Session`]。
pub async fn pull(url: &str, user: &str, pass: &str) -> Result<Vec<u8>> {
    let _ticket = take_ticket()?;
    pull_inner(url, user, pass, TIMEOUT).await
}

/// 占闸门，被占用时报错而不是排队。
fn take_ticket() -> Result<Ticket> {
    try_acquire().ok_or_else(|| anyhow!("{}", rcgui_i18n::t("已有一次备份同步在进行，请等它结束")))
}

/// 真正发请求的部分，不管闸门 —— 闸门由调用方持有。
async fn pull_inner(url: &str, user: &str, pass: &str, timeout: Duration) -> Result<Vec<u8>> {
    check_url(url)?;
    let mut req = client_with(timeout)?.get(url.trim());
    if !user.is_empty() || !pass.is_empty() {
        req = req.basic_auth(user, Some(pass));
    }
    let mut resp = req.send().await.map_err(|e| transport_error(url, &e))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        bail!(
            "{}",
            rcgui_i18n::t("服务器拒绝了凭据（401）：请检查用户名和密码")
        );
    }
    if status.is_redirection() {
        bail!(
            "{}",
            rcgui_i18n::f(
                "{} 返回了重定向，已拒绝跟随（可能是明文降级或别的域名）：请直接填最终地址",
                &[&(redact(url))]
            )
        );
    }
    if !status.is_success() {
        bail!(
            "{}",
            rcgui_i18n::f("下载失败：服务器返回 {}", &[&(status.as_u16())])
        );
    }
    // Content-Length 只是个提示：分块响应根本没有它，所以下面仍然边读边数。
    if let Some(len) = resp.content_length() {
        if len > MAX_BYTES {
            bail!(
                "{}",
                rcgui_i18n::f(
                    "远端文件 {} 字节，超过上限 {} 字节",
                    &[&(len), &(MAX_BYTES)]
                )
            );
        }
    }
    let mut out: Vec<u8> = Vec::new();
    loop {
        let chunk = resp.chunk().await.map_err(|e| transport_error(url, &e))?;
        let Some(chunk) = chunk else {
            break;
        };
        if out.len() as u64 + chunk.len() as u64 > MAX_BYTES {
            // 立刻停手：再读下去就是把内存交给远端控制。
            bail!(
                "{}",
                rcgui_i18n::f("远端文件超过上限 {} 字节，已中止下载", &[&(MAX_BYTES)])
            );
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// PUT 一份配置。
///
/// 同 [`pull`]：自己占闸门。用 [`Session`] 时不会来这里，避免自己和自己抢。
pub async fn push(url: &str, user: &str, pass: &str, body: Vec<u8>) -> Result<()> {
    let _ticket = take_ticket()?;
    push_inner(url, user, pass, body).await
}

/// 真正发 PUT 的部分，不管闸门 —— 闸门由调用方持有。
async fn push_inner(url: &str, user: &str, pass: &str, body: Vec<u8>) -> Result<()> {
    check_url(url)?;
    if body.len() as u64 > MAX_BYTES {
        bail!(
            "{}",
            rcgui_i18n::f(
                "配置 {} 字节，超过上限 {} 字节",
                &[&(body.len()), &(MAX_BYTES)]
            )
        );
    }
    let mut req = client()?
        .put(url.trim())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    if !user.is_empty() || !pass.is_empty() {
        req = req.basic_auth(user, Some(pass));
    }
    let resp = req.send().await.map_err(|e| transport_error(url, &e))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        bail!(
            "{}",
            rcgui_i18n::t("服务器拒绝了凭据（401）：请检查用户名和密码")
        );
    }
    if status.is_redirection() {
        bail!(
            "{}",
            rcgui_i18n::f(
                "{} 返回了重定向，已拒绝跟随：请直接填最终地址",
                &[&(redact(url))]
            )
        );
    }
    if !status.is_success() {
        bail!(
            "{}",
            rcgui_i18n::f(
                "上传失败：服务器返回 {}（WebDAV 目录需要已存在）",
                &[&(status.as_u16())]
            )
        );
    }
    Ok(())
}

/// 网络层错误的文案。地址一律抹掉凭据和查询串再往外说。
fn transport_error(url: &str, e: &reqwest::Error) -> anyhow::Error {
    if e.is_timeout() {
        anyhow!("{}", rcgui_i18n::f("连接 {} 超时", &[&(redact(url))]))
    } else {
        anyhow!(
            "{}",
            rcgui_i18n::f("连接 {} 失败：{}", &[&(redact(url)), &(e)])
        )
    }
}

/// 错误信息里显示的地址：去掉可能被塞进 URL 的用户名密码，以及 query/fragment。
///
/// query 里经常藏着 token（预签名链接就是这么传的），fragment 同理，都不能进日志。
pub fn strip_userinfo(url: &str) -> String {
    let u = url.trim();
    let Some(scheme_end) = u.find("://") else {
        return u.split(['?', '#']).next().unwrap_or("").to_string();
    };
    let rest = &u[scheme_end + 3..];
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    let host = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    let tail = tail.split(['?', '#']).next().unwrap_or("");
    format!("{}://{}{}", &u[..scheme_end], host, tail)
}

/// 往日志/界面里放地址时统一用它。
fn redact(url: &str) -> String {
    strip_userinfo(url)
}

/// 供界面显示：`push` 成功后写到哪。
pub fn describe_target(url: &str) -> String {
    let u = strip_userinfo(url);
    if u.is_empty() {
        return rcgui_i18n::t("（未填）").to_string();
    }
    u
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[test]
    fn only_https_or_loopback_http() {
        assert!(check_url("https://dav.example.com/rcgui/rcgui.conf").is_ok());
        assert!(check_url("http://127.0.0.1:8080/rcgui.conf").is_ok());
        assert!(check_url("http://localhost/rcgui.conf").is_ok());
        assert!(check_url("http://[::1]:8080/x").is_ok());
        assert!(check_url("http://dav.example.com/x").is_err());
        assert!(check_url("ftp://dav.example.com/x").is_err());
        assert!(check_url("").is_err());
        assert!(check_url("https://").is_err());
    }

    #[test]
    fn check_url_rejects_userinfo() {
        // 凭据写在 URL 里会随日志、错误信息外泄。
        assert!(check_url("https://bob:secret@dav.example.com/x").is_err());
        assert!(check_url("https://bob@dav.example.com/x").is_err());
        // 路径里的 @ 不是 userinfo。
        assert!(check_url("https://dav.example.com/a@b").is_ok());
        assert!(check_url("http://127.0.0.1:8080/a@b").is_ok());
    }

    /// 备份闸门（`GATE`）是进程级单例，整个测试二进制只有一把。凡是会占用它的用例
    /// ——`Session`、自由 `pull`/`push`、直接 `try_acquire`——都要先拿这把测试锁排队，
    /// 否则并行执行时互相抢，报出来的"已有一次备份同步在进行"看着像生产缺陷，
    /// 其实只是用例互撞。排队只解决互撞：每个用例自己仍然断言"占着的时候第二次申请必须失败"。
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        // 用例 panic 会毒化这把锁，但它不保护任何数据，取回内部数据继续用。
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn gate_rejects_second_acquire_and_recovers_after_drop() {
        let _serial = serial();
        let first = try_acquire().expect("第一次应成功");
        assert!(try_acquire().is_none(), "第二次必须被挡住");
        drop(first);
        assert!(try_acquire().is_some(), "释放后应能再次占用");
    }

    #[test]
    fn session_covers_request_and_save() {
        let _serial = serial();
        let s = Session::begin().expect("第一次应成功");
        assert!(Session::begin().is_err(), "同一时刻只能有一次同步");
        // 保存失败必须返回 Err，不能报成功。数据目录位置先占成一个文件，
        // 这样 `<数据目录>/backups` 建不出来。
        let base = std::env::temp_dir().join(format!("rcgui-bk-blocked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::write(&base, b"not a dir").unwrap();
        let bad = s.save(&base, "rcgui.conf", b"{}");
        assert!(bad.is_err(), "目录建不出来时不能报成功");
        let _ = std::fs::remove_file(&base);
        // 同步期间仍然占着门，drop 之后才放行。
        drop(s);
        assert!(Session::begin().is_ok());
    }

    #[tokio::test]
    async fn free_functions_take_the_gate() {
        // 不经 Session 直接调 pull/push 也必须占闸门：否则会话外面随便一个请求
        // 就能和正在进行的上传下载叠在一起，改同一个远端配置。
        // 持着测试锁时闸门必然空着，抢不到就是真失败，不能靠等待掩盖。
        let _serial = serial();
        let held = try_acquire().expect("闸门应该空着");
        let err = pull("https://dav.example.com/x", "", "")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("已有一次备份同步在进行"), "{err}");
        let err = push("https://dav.example.com/x", "", "", b"{}".to_vec())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("已有一次备份同步在进行"), "{err}");
        drop(held);
        // 放下之后必须真的放开，不能因为抢不到就永久锁死。
        assert!(try_acquire().is_some(), "闸门没有被放开");
    }

    #[test]
    fn local_name_is_sanitized() {
        assert_eq!(
            local_name_for("https://dav.example.com/a/rcgui.conf"),
            "rcgui.conf"
        );
        assert_eq!(local_name_for("https://e.com/a/../../etc/passwd"), "passwd");
        assert_eq!(local_name_for("https://e.com/a/x%20y.conf"), "x_20y.conf");
        assert_eq!(local_name_for("https://e.com/"), "rcgui.conf");
        // 不能产生空名或只剩点号（Windows 上不可写）
        assert_eq!(local_name_for("https://e.com/a/.."), "rcgui.conf");
    }

    #[test]
    fn strip_userinfo_hides_credentials() {
        assert_eq!(
            strip_userinfo("https://bob:secret@dav.example.com/x"),
            "https://dav.example.com/x"
        );
        assert_eq!(
            strip_userinfo("https://dav.example.com/a@b"),
            "https://dav.example.com/a@b"
        );
        assert_eq!(strip_userinfo("dav.example.com/x"), "dav.example.com/x");
    }

    #[test]
    fn strip_userinfo_drops_query_and_fragment() {
        // 预签名链接把凭据放在 query 里，不能进错误信息。
        assert_eq!(
            strip_userinfo("https://dav.example.com/x?token=abc#frag"),
            "https://dav.example.com/x"
        );
        assert_eq!(
            strip_userinfo("https://u:p@dav.example.com/x?token=abc"),
            "https://dav.example.com/x"
        );
        assert_eq!(
            strip_userinfo("dav.example.com/x?token=abc"),
            "dav.example.com/x"
        );
    }

    #[test]
    fn save_local_copy_stays_inside_dir() {
        let dir = std::env::temp_dir().join(format!("rcgui-bk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = save_local_copy(
            &dir,
            &local_name_for("https://e.com/../../evil.conf"),
            b"{}",
        )
        .unwrap();
        assert!(p.starts_with(local_dir(&dir)), "{}", p.display());
        assert!(std::fs::read(&p).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_local_copy_refuses_path_in_name() {
        let dir = std::env::temp_dir().join(format!("rcgui-bk-trav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 名字里带路径、带 `..`、带盘符、空名字，一律报错；不静默改名。
        for bad in [
            "../../escape.conf",
            "a/b.conf",
            "..\\..\\x.conf",
            "C:\\evil.conf",
            "..",
            ".",
            "",
            "   ",
        ] {
            let err = save_local_copy(&dir, bad, b"{}");
            assert!(err.is_err(), "{:?} 应该被拒绝", bad);
        }
        // 一个文件都不该写出来，更不能写到 backups 目录外面。
        let backups = local_dir(&dir);
        if backups.is_dir() {
            assert_eq!(
                std::fs::read_dir(&backups).unwrap().count(),
                0,
                "坏名字不该留下文件"
            );
        }
        assert!(!std::env::temp_dir().join("escape.conf").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_url_errors_are_redacted() {
        // 用户可能把凭据填进地址里，报错时一个字都不能带走。
        let err = check_url("http://user:secret@dav.example.com/x?token=abc")
            .unwrap_err()
            .to_string();
        assert!(!err.contains("secret"), "{}", err);
        assert!(!err.contains("token"), "{}", err);
        assert!(err.contains("dav.example.com"), "{}", err);

        let err = check_url("ftp://u:p@dav.example.com/x?token=abc")
            .unwrap_err()
            .to_string();
        assert!(!err.contains("p@"), "{}", err);
        assert!(!err.contains("token"), "{}", err);
    }

    // ---- 本地假 HTTP 服务：验证限流、认证失败、重定向、超时 ----

    struct Mock {
        base: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn serve<F, Fut>(handler: F) -> Mock
    where
        F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    // 每个连接交给处理函数自己读完请求再回。
                    handler(sock).await;
                });
            }
        });
        Mock {
            base: format!("http://{addr}/rcgui.conf"),
            task,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_pull_runs_inside_spawned_task() {
        let m = serve(|mut sock| async move {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let body = b"{\"ok\":1}";
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body).await;
            let _ = sock.flush().await;
        })
        .await;
        let _serial = serial();
        let url = m.base.clone();
        // 这段能不能编过就是证据：闸门类型不是 Send 的话，spawn 要求 Future: Send
        // 会直接编译失败。测试里再顺带验证它真能拉下来。
        let got = tokio::spawn(async move {
            let session = Session::begin().expect("闸门应该空着");
            session.pull(&url, "", "").await.unwrap()
        })
        .await
        .unwrap();
        assert_eq!(got, b"{\"ok\":1}");
        // 任务结束后闸门应该已经放开。
        assert!(try_acquire().is_some());
    }

    #[tokio::test]
    async fn pull_returns_body() {
        let m = serve(|mut sock| async move {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let body = b"{\"a\":1}";
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body).await;
            let _ = sock.shutdown().await;
        })
        .await;
        let bytes = pull_inner(&m.base, "", "", TIMEOUT).await.unwrap();
        assert_eq!(bytes, b"{\"a\":1}");
    }

    #[tokio::test]
    async fn pull_stops_on_chunked_over_limit() {
        // 没有 Content-Length 的分块响应必须边读边数，超了立刻中止。
        let m = serve(|mut sock| async move {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await;
            let piece = vec![b'x'; 1024 * 1024];
            for _ in 0..6 {
                let head = format!("{:x}\r\n", piece.len());
                if sock.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                if sock.write_all(&piece).await.is_err() {
                    return;
                }
                if sock.write_all(b"\r\n").await.is_err() {
                    return;
                }
            }
            let _ = sock.write_all(b"0\r\n\r\n").await;
        })
        .await;
        let err = pull_inner(&m.base, "", "", TIMEOUT)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("上限"), "{err}");
    }

    #[tokio::test]
    async fn pull_reports_401() {
        let m = serve(|mut sock| async move {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        })
        .await;
        let err = pull_inner(&m.base, "u", "p", TIMEOUT)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "{err}");
    }

    #[tokio::test]
    async fn pull_refuses_redirect() {
        // 302 到别的域（或明文）会泄漏凭据，必须报错而不是跟过去。
        let m = serve(|mut sock| async move {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://evil.example/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        })
        .await;
        let err = pull_inner(&m.base, "u", "p", TIMEOUT)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("重定向"), "{err}");
    }

    #[tokio::test]
    async fn pull_times_out() {
        // 服务端接了连接却不说话：超时必须变成错误，而不是永远挂着。
        let m = serve(|mut sock| async move {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
        .await;
        let err = pull_inner(&m.base, "", "", Duration::from_millis(300))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("超时"), "{err}");
    }

    #[tokio::test]
    async fn push_rejects_oversize_body_before_network() {
        let big = vec![0u8; (MAX_BYTES + 1) as usize];
        let err = push_inner("https://dav.example.com/x", "", "", big)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("上限"), "{err}");
    }
}
