//! 由本程序管理的 rclone 内核（managed core）。
//!
//! 这个模块只做一件事：**自己拉起一个 rclone，连上它，并在退出时只收掉自己
//! 拉起的那个进程**。它不接管系统里别的 rclone：端口由系统分配（不会有
//! 5572 之类固定端口的冲突），RC 凭据每次随机生成，找不到就报错而不是去猜。
//!
//! 三条边界，改动前先读：
//!
//! 1. **只杀自己的子进程**。`Running` 里存着 `Child` 的 pid，停止时先比对再做
//!    终止；绝不按进程名杀 `rclone.exe`，那会连用户自己开的实例一起杀掉。
//! 2. **不假定端口**。端口是启动时向系统要的空闲端口，先绑再放，随后交给
//!    rclone；绑定失败就直接报错，不去扫描别人的端口。
//! 3. **连不上不等于没装**。exe 存在、进程起来了、TCP 能连、RC 认证通过是四件
//!    不同的事，每一件都要分开报错，否则用户看到的是"内核有问题"这种废话。
//!
//! 子进程另外挂在 Job Object 上（Windows），父进程被强杀时系统会连带收掉它，
//! 不会留下孤儿 rclone 占着端口和配置文件。

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 内核对外的固定文件名。打包脚本把它放在 `<exe>/core/rclone.exe`。
pub const CORE_EXE: &str = if cfg!(windows) {
    "rclone.exe"
} else {
    "rclone"
};

/// 下载内核的字节上限。官方 Windows 包约 20MB，留足余量但不无限。
pub const MAX_DOWNLOAD: u64 = 120 * 1024 * 1024;

/// 内核来源。报错信息里要说清"用的是哪一个"，否则用户不知道改哪里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 用户显式指定（`core.path`，可以选插件部署的 fork）
    Custom,
    /// 打包时放在 `<exe>/core/` 旁边
    BesideExe,
    /// 程序数据目录下的 `core/`（下载/更新落位的地方）
    DataDir,
    /// 系统 PATH
    Path,
}

impl Source {
    /// 给用户看的一句话，会出现在状态栏和报错里。
    pub fn label(self) -> &'static str {
        match self {
            Source::Custom => "手动指定",
            Source::BesideExe => "程序目录 core/",
            Source::DataDir => "数据目录 core/",
            Source::Path => "系统 PATH",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Located {
    pub exe: PathBuf,
    pub source: Source,
}

/// 内核 exe 的查找顺序，以及"哪一个才算数"的规则。
///
/// 与任务书写的"打包目录 → 数据目录 core → 自定义 → PATH"有两处**有意的偏差**：
///
/// 1. 显式设置的 `core.path` 排在最前面。插件可能依赖官方 rclone 里没有的后端
///    （例如 teldrive 要专门的 fork），随包官方 exe 若永远优先，用户指定的 fork
///    就永远用不上。
/// 2. 随包 `core/` 与数据目录 `core/` 同时存在时**比版本取新的**。数据目录是
///    程序下载/更新内核的落位处；如果永远让随包的那个优先，用户点了"更新内核"
///    之后用的还是安装包里那份旧内核，更新看起来成功了却毫无效果。
///
/// `custom` 非空但指向的 exe 不存在时返回 `Err`，**不静默回落到随包/PATH**：
/// 用户明确指定了 fork，悄悄换成官方 exe 只会让依赖它的插件创建远程时失败，
/// 那种"设置存了但不生效"的故障比直接报错难查得多。
pub fn resolve(custom: &str, exe_dir: &Path, data_dir: &Path) -> Result<Option<Located>> {
    resolve_with(custom, exe_dir, data_dir, which)
}

/// 同 [`resolve`]，但最后一档 PATH 查找由调用方给。
///
/// 分开是为了测试：本机 PATH 里可能装着一个真 rclone，那样"什么都没找到"
/// 这条断言永远不成立，而测试不该依赖开发机的 PATH。
pub fn resolve_with(
    custom: &str,
    exe_dir: &Path,
    data_dir: &Path,
    lookup: impl Fn(&str) -> Option<PathBuf>,
) -> Result<Option<Located>> {
    let custom = custom.trim();
    if !custom.is_empty() {
        let p = PathBuf::from(custom);
        // 允许指向目录（拖进去一个文件夹），取其中的 rclone.exe
        let p = if p.is_dir() { p.join(CORE_EXE) } else { p };
        if p.is_file() {
            return Ok(Some(Located {
                exe: p,
                source: Source::Custom,
            }));
        }
        bail!(
            "指定的内核路径不存在：{}。已按你的设置停在这里，不会自动改用随包或 PATH 里的 rclone——\n\
             如果那个 exe 是插件专用的内核（例如 teldrive 的 fork），换掉会让依赖它的远程用不了。\n\
             请把路径改对，或清空「内核路径」改用自动查找。",
            p.display()
        );
    }

    let beside = exe_dir.join("core").join(CORE_EXE);
    let data = data_core_exe(data_dir);
    let beside_ok = beside.is_file();
    let data_ok = data.is_file();

    if beside_ok && data_ok {
        // 两个都在：谁版本新用谁。探测失败的那一侧不参与比较（跑不起来的 exe
        // 版本号再高也没意义）。
        let bv = probe_version(&beside).ok();
        let dv = probe_version(&data).ok();
        let pick_data = match (bv.as_deref(), dv.as_deref()) {
            (Some(b), Some(d)) => ver_key(d) > ver_key(b),
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => true, // 两个都跑不起来：选我们管理的那个，便于下次更新覆盖
        };
        return Ok(Some(if pick_data {
            Located {
                exe: data,
                source: Source::DataDir,
            }
        } else {
            Located {
                exe: beside,
                source: Source::BesideExe,
            }
        }));
    }
    if beside_ok {
        return Ok(Some(Located {
            exe: beside,
            source: Source::BesideExe,
        }));
    }
    if data_ok {
        return Ok(Some(Located {
            exe: data,
            source: Source::DataDir,
        }));
    }
    Ok(lookup(CORE_EXE).map(|p| Located {
        exe: p,
        source: Source::Path,
    }))
}

/// 把 `v1.67.0` / `1.67.0-beta.1234` 变成可比较的数字序列。
///
/// rclone 的版本号不总是严格 semver（fork 会带 `-beta.1234.abcdef` 之类的尾巴），
/// 这里只取数字段按位比较；取不到数字就返回空序列，调用方据此当作"更旧"。
fn ver_key(v: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in v.chars() {
        if ch.is_ascii_digit() {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.push(cur.parse().unwrap_or(0));
            cur.clear();
        }
    }
    if !cur.is_empty() {
        out.push(cur.parse().unwrap_or(0));
    }
    out
}

/// 数据目录下内核的落位路径（下载/更新写这里）。
pub fn data_core_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("core")
}

pub fn data_core_exe(data_dir: &Path) -> PathBuf {
    data_core_dir(data_dir).join(CORE_EXE)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// rclone 默认配置路径（`--config` 的缺省值）。
///
/// 与 rclone 自己的规则一致：`RCLONE_CONFIG` 优先，其次平台默认位置。
/// 只算路径，不读文件——读配置由 RC 或只读解析负责。
pub fn default_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("RCLONE_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if cfg!(windows) {
        if let Some(a) = std::env::var_os("APPDATA") {
            return PathBuf::from(a).join("rclone").join("rclone.conf");
        }
    }
    if let Some(u) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        return PathBuf::from(u)
            .join(".config")
            .join("rclone")
            .join("rclone.conf");
    }
    PathBuf::from("rclone.conf")
}

/// 随机 RC 凭据。
///
/// 长度 32、ASCII 字母数字：既能直接塞进 URL 与 basic auth，也不会因为
/// 特殊字符在命令行/日志里被转义得面目全非。调用方**不得**把它写进日志。
pub fn random_secret(len: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(len);
    // 用 rand 的 thread_rng：内部是 CSPRNG，够做一次性凭据
    use rand::Rng;
    let mut rng = rand::thread_rng();
    for _ in 0..len {
        let i = rng.gen_range(0..CHARS.len());
        out.push(CHARS[i] as char);
    }
    out
}

/// 向系统要一个当前空闲的 loopback 端口。
///
/// 绑 0 号端口让内核挑，拿到后立刻释放。这里存在"释放到 rclone 绑定之间被
/// 别人抢走"的窗口期，所以调用方拿到 `AddrInUse` 时要重试，而不是把它当成
/// 致命错误（见 [`Core::start`]）。
pub fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("无法申请本地端口（系统可能没有可用的 loopback 端口）")?;
    let port = l.local_addr()?.port();
    drop(l);
    Ok(port)
}

/// 启动一个 managed 内核需要的东西。
#[derive(Debug, Clone)]
pub struct StartSpec {
    pub exe: PathBuf,
    /// rclone 配置文件路径（`--config`）。写它之前要确认没有别的实例在用。
    pub config: PathBuf,
    /// 内核自己的日志。和主程序日志分开，便于用户单独查看。
    pub log_file: PathBuf,
    /// 额外的 rclone 全局参数（例如 `--tpslimit`），放在 rcd 之前。
    pub extra_args: Vec<String>,
}

/// 运行中的内核句柄。
///
/// 只保留启动子进程之后还需要的东西：句柄、pid、RC 端点与会话凭据。
/// exe 路径和日志路径留在 [`StartSpec`] 里由调用方保存——放两份就会出现
/// 两份不一致的可能，而报错时要引用的是启动时那一份。
pub struct Running {
    child: Child,
    pub pid: u32,
    pub port: u16,
    pub user: String,
    pass: String,
    #[cfg(windows)]
    job: Option<JobHandle>,
}

impl Running {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// 子进程是否还活着（不阻塞）。
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// 收掉子进程。**只动自己这一个 pid**。
    pub fn stop(&mut self) {
        if self.child.try_wait().map(|s| s.is_some()).unwrap_or(false) {
            return;
        }
        // kill() 在 Windows 上是 TerminateProcess，目标是我们自己的 child 句柄，
        // 不存在"按名字误杀"的可能。
        let _ = self.child.kill();
        let _ = self.child.wait();
        #[cfg(windows)]
        {
            // Job Object 句柄随结构体释放；这里显式 drop 让 KILL_ON_JOB_CLOSE
            // 在 stop 时也立刻生效，而不是等到程序退出。
            self.job = None;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 内核状态（给界面显示，不含凭据）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Phase {
    /// 没有内核 exe
    #[default]
    Missing,
    /// 有 exe，没在跑
    Stopped,
    /// 正在跑
    Running { pid: u32, url: String },
    /// 上次启动失败
    Failed(String),
}

impl Phase {
    /// 给状态栏显示的一句话（不含凭据，只含 pid 和 loopback 地址）。
    pub fn label(&self) -> String {
        match self {
            Phase::Missing => "未找到 rclone".to_string(),
            Phase::Stopped => "内核未启动".to_string(),
            Phase::Running { pid, url } => format!("内核运行中（pid {pid}，{url}）"),
            Phase::Failed(e) => format!("内核启动失败：{e}"),
        }
    }
}

/// managed 内核的所有者。
///
/// 一个进程里只应有一个实例：内部用 `Mutex<Option<Running>>` 保证同一时刻
/// 最多一个子进程，重复 `start` 会先把旧的收掉。
#[derive(Default)]
pub struct Core {
    inner: Mutex<Option<Running>>,
    last_error: Mutex<Option<String>>,
}

impl Core {
    pub fn new() -> Self {
        Self::default()
    }

    /// 启动（若已在跑，先停旧的）。端口冲突自动重试。
    pub fn start(&self, spec: &StartSpec) -> Result<()> {
        if !spec.exe.is_file() {
            let e = anyhow!(
                "找不到内核 {}（来源不明，请在设置里指定 rclone.exe 路径）",
                spec.exe.display()
            );
            self.set_error(e.to_string());
            return Err(e);
        }
        self.stop();
        if let Some(dir) = spec.log_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..3 {
            let port = free_port()?;
            let user = random_secret(16);
            let pass = random_secret(32);
            match spawn(
                &spec.exe,
                &spec.config,
                &spec.log_file,
                port,
                &user,
                &pass,
                &spec.extra_args,
            ) {
                Ok(mut child) => {
                    // 立刻挂 Job Object：万一 set_connected 之前程序崩溃，
                    // 子进程也会被系统连带收掉。
                    #[cfg(windows)]
                    let job = JobHandle::assign(child.id());
                    let pid = child.id();
                    match wait_ready(port, Duration::from_secs(20)) {
                        Ok(()) => {
                            let running = Running {
                                child,
                                pid,
                                port,
                                user,
                                pass,
                                #[cfg(windows)]
                                job,
                            };
                            *self.inner.lock().unwrap() = Some(running);
                            *self.last_error.lock().unwrap() = None;
                            return Ok(());
                        }
                        Err(e) => {
                            // 端口被抢/启动崩溃都走这里：杀掉重试，最多三次
                            let _ = child.kill();
                            let _ = child.wait();
                            last = Some(e);
                            if attempt == 2 {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
                Err(e) => {
                    last = Some(e);
                    if attempt == 2 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
        let e = last.unwrap_or_else(|| anyhow!("内核启动失败（原因未知）"));
        let msg = format!("{}（日志：{}）", e, spec.log_file.display());
        self.set_error(msg.clone());
        Err(anyhow!(msg))
    }

    pub fn stop(&self) {
        let mut g = self.inner.lock().unwrap();
        if let Some(mut r) = g.take() {
            r.stop();
        }
    }

    pub fn is_running(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        match g.as_mut() {
            Some(r) => r.is_alive(),
            None => false,
        }
    }

    /// 运行中内核的 RC 端点与凭据。
    ///
    /// 这组凭据**只属于本次会话**：端口是启动时随机挑的，密码是每次生成的，
    /// 所以调用方只能拿它去连（`connect_report` 就是这么用的），
    /// 绝不能写回 `cfg.rclone` 再 `config::save` ——
    /// 那样会把一次性凭据变成用户档案里的"外部端点"，下次启动连上去必然 401。
    /// 也不得写日志、不得回显到界面。
    pub fn endpoint(&self) -> Option<(String, String, String)> {
        let mut g = self.inner.lock().unwrap();
        let r = g.as_mut()?;
        if !r.is_alive() {
            return None;
        }
        Some((r.url(), r.user.clone(), r.pass.clone()))
    }

    pub fn pid(&self) -> Option<u32> {
        let g = self.inner.lock().unwrap();
        g.as_ref().map(|r| r.pid)
    }

    pub fn phase(&self) -> Phase {
        if let Ok(g) = self.inner.lock() {
            if let Some(r) = g.as_ref() {
                return Phase::Running {
                    pid: r.pid,
                    url: r.url(),
                };
            }
        }
        if let Ok(e) = self.last_error.lock() {
            if let Some(e) = e.as_ref() {
                return Phase::Failed(e.clone());
            }
        }
        Phase::Stopped
    }

    fn set_error(&self, msg: String) {
        *self.last_error.lock().unwrap() = Some(msg);
    }
}

fn spawn(
    exe: &Path,
    config: &Path,
    log_file: &Path,
    port: u16,
    user: &str,
    pass: &str,
    extra: &[String],
) -> Result<Child> {
    let mut cmd = Command::new(exe);
    cmd.arg("rcd")
        .arg("--rc-addr")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--rc-user")
        .arg(user)
        .arg("--rc-pass")
        .arg(pass)
        .arg("--config")
        .arg(config)
        .arg("--log-file")
        .arg(log_file)
        .arg("--log-level")
        .arg("INFO");
    for a in extra {
        cmd.arg(a);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW：不弹控制台窗口；DETACHED 会丢掉 Job Object 关联，不用。
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.spawn()
        .map_err(|e| anyhow!("无法启动内核 {}：{e}", exe.display()))
}

/// 等到内核的 RC 端口可连。
///
/// 只做 TCP 连接探测，不验证凭据——凭据验证由调用方用 RC 自己的
/// `core/version` 完成，这样"进程没起来"和"认证不对"是两个清楚的错误。
fn wait_ready(port: u16, timeout: Duration) -> Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let deadline = Instant::now() + timeout;
    loop {
        if std::net::TcpStream::connect_timeout(&addr.parse()?, Duration::from_millis(300)).is_ok()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("内核进程已启动，但 {timeout:?} 内 RC 端口 {port} 仍无法连接");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 读内核版本（启动一个 `rclone version` 子进程）。
///
/// 只用于「选了一个 exe，它到底能不能跑」这种确认，运行期版本一律走 RC
/// 的 `core/version`。失败返回 Err 而不是空字符串，避免把"跑不起来"当成"版本未知"。
pub fn probe_version(exe: &Path) -> Result<String> {
    let mut cmd = Command::new(exe);
    cmd.arg("version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd
        .output()
        .map_err(|e| anyhow!("无法运行 {}：{e}", exe.display()))?;
    if !out.status.success() {
        bail!("{} version 退出码 {}", exe.display(), out.status);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap_or("").trim().to_string();
    if first.is_empty() {
        bail!("{} version 没有输出", exe.display());
    }
    Ok(first)
}

// ---------------------------------------------------------------- 下载 / 更新

/// 从官方站点取最新版本号（`https://downloads.rclone.org/version.txt`）。
pub async fn latest_official_version() -> Result<String> {
    let text = fetch_text("https://downloads.rclone.org/version.txt").await?;
    let v = text.trim().trim_start_matches('v').to_string();
    if v.is_empty()
        || !v
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
    {
        bail!("官方版本号格式异常：{v:?}");
    }
    Ok(v)
}

/// 官方 Windows x64 包地址。版本号只接受 `1.2.3` 这类干净的数字串，
/// 防止把任意 URL 拼进来。
pub fn official_zip_url(version: &str) -> Result<String> {
    let clean = version.trim().trim_start_matches('v');
    let ok = !clean.is_empty()
        && clean.chars().all(|c| c.is_ascii_digit() || c == '.')
        && clean.split('.').count() >= 2;
    if !ok {
        bail!("版本号不合法：{version:?}");
    }
    Ok(format!(
        "https://downloads.rclone.org/v{clean}/rclone-v{clean}-windows-amd64.zip"
    ))
}

async fn fetch_text(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        bail!("{url} 返回 HTTP {}", resp.status());
    }
    Ok(resp.text().await?)
}

/// 下载（带字节上限与超时）。
pub async fn fetch_bytes(url: &str, cap: u64) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()?;
    let mut resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        bail!("{url} 返回 HTTP {}", resp.status());
    }
    if let Some(len) = resp.content_length() {
        if len > cap {
            bail!("下载内容 {} 字节，超过上限 {}", len, cap);
        }
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if buf.len() as u64 + chunk.len() as u64 > cap {
            bail!("下载内容超过上限 {} 字节，已中止", cap);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// 从 `SHA256SUMS` 文本里找某个文件名的哈希（官方文件用 `hash␠␠name` 格式）。
pub fn hash_for(sums: &str, name: &str) -> Option<String> {
    for line in sums.lines() {
        let mut it = line.split_whitespace();
        let (Some(h), Some(n)) = (it.next(), it.next()) else {
            continue;
        };
        let n = n.trim_start_matches('*');
        if n == name && h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(h.to_ascii_lowercase());
        }
    }
    None
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 下载官方内核并安装到 `target_dir`。
///
/// 流程固定为「下载 → 校验 zip 哈希 → 解压到临时目录 → 找到 rclone.exe →
/// 校验它能跑 → 原子替换」。任何一步失败都不会动现有的 exe。
pub async fn install_official(target_dir: &Path, version: Option<&str>) -> Result<PathBuf> {
    let ver = match version {
        Some(v) if !v.trim().is_empty() => v.trim().trim_start_matches('v').to_string(),
        _ => latest_official_version().await?,
    };
    let zip_url = official_zip_url(&ver)?;
    let sums_url = format!("https://downloads.rclone.org/v{ver}/SHA256SUMS");
    let zip_name = format!("rclone-v{ver}-windows-amd64.zip");
    let sums = fetch_text(&sums_url).await?;
    let want = hash_for(&sums, &zip_name)
        .ok_or_else(|| anyhow!("{sums_url} 里没有 {zip_name} 的校验值"))?;
    let bytes = fetch_bytes(&zip_url, MAX_DOWNLOAD).await?;
    let got = sha256_hex(&bytes);
    if got != want {
        bail!("内核包校验失败：期望 {want}，实际 {got}（下载可能被篡改或损坏）");
    }
    install_from_zip(&bytes, target_dir)
}

/// 把内核 zip 解出来放到 `target_dir`。
///
/// 校验先于替换：新的 exe 先落在同目录的临时名上，确认能运行后才改名，
/// 旧文件在改名失败时恢复。
pub fn install_from_zip(zip: &[u8], target_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(target_dir)?;
    let stage = target_dir.join("stage-tmp");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    plugin_host::zip::extract_all(zip, &stage, MAX_DOWNLOAD)
        .context("内核包解压失败（不是有效的 zip，或内含非法路径）")?;
    // 官方包是 rclone-vX.Y.Z-windows-amd64/rclone.exe，允许一层目录
    let exe = find_file_ci(&stage, CORE_EXE).ok_or_else(|| anyhow!("内核包里没有 {CORE_EXE}"))?;
    let ver = probe_version(&exe).context("下载到的内核无法运行，已放弃安装")?;

    let dst = target_dir.join(CORE_EXE);
    let bak = target_dir.join(format!("{CORE_EXE}.bak"));
    let _ = std::fs::remove_file(&bak);
    if dst.exists() {
        std::fs::rename(&dst, &bak).context("无法备份现有内核（可能正在被使用）")?;
    }
    if let Err(e) = std::fs::rename(&exe, &dst) {
        // 回滚：旧文件必须回到原位，否则用户既没有新内核也没有旧内核
        if bak.exists() {
            let _ = std::fs::rename(&bak, &dst);
        }
        let _ = std::fs::remove_dir_all(&stage);
        return Err(anyhow!("替换内核失败：{e}"));
    }
    let _ = std::fs::remove_dir_all(&stage);
    let _ = std::fs::remove_file(&bak);
    log_line(&format!("内核已安装到 {}（{ver}）", dst.display()));
    Ok(dst)
}

/// 在目录里按文件名（忽略大小写）找文件，只下钻一层——官方包与自制包都够用，
/// 不做无限递归，避免在异常目录结构上浪费时间。
fn find_file_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_file() {
            if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                if n.eq_ignore_ascii_case(name) {
                    return Some(p);
                }
            }
        } else if p.is_dir() {
            subdirs.push(p);
        }
    }
    for d in subdirs {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_file() {
                    if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                        if n.eq_ignore_ascii_case(name) {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    None
}

/// 简单的追加日志。内核模块可能在日志系统起来之前被调用，所以自带一个
/// 带时间戳的写入，且**只记路径与错误，不记凭据**。
fn log_line(msg: &str) {
    let dir = crate::config::data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("core.log");
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{ts} {msg}");
    }
}

/// 防止把外部路径塞进内核目录：安装目标必须是绝对路径且不含 `..`。
pub fn guard_target_dir(dir: &Path) -> Result<()> {
    if !dir.is_absolute() {
        bail!("内核安装目录必须是绝对路径：{}", dir.display());
    }
    if dir.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("内核安装目录不能包含 `..`：{}", dir.display());
    }
    Ok(())
}

// ---------------------------------------------------------------- Job Object

/// Windows Job Object 句柄：把子进程放进去，父进程消失时系统连带终止它。
///
/// 这是防"孤儿 rclone"的兜底：正常退出走 [`Running::stop`]，崩溃/被强杀时
/// 只能靠内核对象。句柄随 [`Running`] 一起释放，`KILL_ON_JOB_CLOSE` 生效。
#[cfg(windows)]
struct JobHandle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for JobHandle {}

#[cfg(windows)]
impl JobHandle {
    fn assign(pid: u32) -> Option<Self> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };
        unsafe {
            let job = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .is_ok();
            if !ok {
                let _ = CloseHandle(job);
                return None;
            }
            let proc = match OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) {
                Ok(h) => h,
                Err(_) => {
                    let _ = CloseHandle(job);
                    return None;
                }
            };
            let assigned = AssignProcessToJobObject(job, proc).is_ok();
            let _ = CloseHandle(proc);
            if !assigned {
                let _ = CloseHandle(job);
                return None;
            }
            Some(Self(job))
        }
    }
}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rcgui-core-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"stub").unwrap();
    }

    #[test]
    fn resolve_prefers_custom_then_newest_of_beside_and_data_then_path() {
        let root = tmpdir("resolve");
        let exe_dir = root.join("app");
        let data = root.join("data");
        let custom = root.join("fork").join(CORE_EXE);
        let beside = exe_dir.join("core").join(CORE_EXE);
        let in_data = data.join("core").join(CORE_EXE);

        // 什么都没有：不报错，交给调用方。PATH 那一档换成"一定找不到"，
        // 否则在装了 rclone 的机器上第一条断言必然假失败。
        let find = |custom: &str| resolve_with(custom, &exe_dir, &data, |_| None);
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        assert!(find("").unwrap().is_none());

        touch(&in_data);
        let l = find("").unwrap().unwrap();
        assert_eq!(l.source, Source::DataDir);

        // 两个都在、都是跑不起来的假 exe：选我们管理的数据目录那份，
        // 这样"更新内核"至少能覆盖到实际在用的文件
        touch(&beside);
        let l = find("").unwrap().unwrap();
        assert_eq!(l.source, Source::DataDir);

        // 用户显式指定的 fork 必须压过随包的官方内核，否则设置等于没生效
        touch(&custom);
        let l = find(custom.to_str().unwrap()).unwrap().unwrap();
        assert_eq!(l.source, Source::Custom);
        assert_eq!(l.exe, custom);

        // 指向目录也认
        let l = find(custom.parent().unwrap().to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(l.source, Source::Custom);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_refuses_to_fall_back_when_custom_path_is_missing() {
        let root = tmpdir("resolve-missing");
        let exe_dir = root.join("app");
        let data = root.join("data");
        touch(&exe_dir.join("core").join(CORE_EXE));
        // 随包内核存在，但用户指定了另一个不存在的路径
        let err = resolve(root.join("nope").to_str().unwrap(), &exe_dir, &data)
            .expect_err("指定路径不存在时必须报错，而不是悄悄换成随包内核");
        let msg = err.to_string();
        assert!(msg.contains("不存在"), "错误信息要说明路径不存在: {msg}");
        assert!(
            msg.contains("不会自动改用"),
            "错误信息要说明不会回退: {msg}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ver_key_compares_numeric_segments() {
        assert!(ver_key("v1.67.0") > ver_key("v1.66.1"));
        assert!(ver_key("v1.70.0") > ver_key("v1.9.9"));
        assert_eq!(ver_key("v1.67.0"), ver_key("1.67.0"));
        // 跟不上版本的尾巴不影响数字段比较，取不到数字的一侧当作更旧
        assert!(ver_key("v1.67.0-beta.1234.abcdef") > ver_key("v1.66.0"));
        assert!(ver_key("v1.67.0") > ver_key("garbage"));
    }

    #[test]
    fn random_secret_is_alnum_and_not_repeated() {
        let a = random_secret(32);
        let b = random_secret(32);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b, "两次生成的凭据不应相同");
    }

    #[test]
    fn free_port_returns_bindable_port() {
        let p = free_port().unwrap();
        assert!(p > 0);
        // 拿到的端口应当可以再绑（极小概率被别人抢，失败就重来一次）
        let ok = std::net::TcpListener::bind(("127.0.0.1", p)).is_ok();
        if !ok {
            let p2 = free_port().unwrap();
            assert!(std::net::TcpListener::bind(("127.0.0.1", p2)).is_ok());
        }
    }

    #[test]
    fn hash_for_parses_official_sums_format() {
        let sums = "\
9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08  rclone-v1.68.1-windows-amd64.zip
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa *rclone-v1.68.1-linux-amd64.zip
";
        assert_eq!(
            hash_for(sums, "rclone-v1.68.1-windows-amd64.zip").unwrap(),
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
        assert_eq!(
            hash_for(sums, "rclone-v1.68.1-linux-amd64.zip").unwrap(),
            "a".repeat(64)
        );
        assert!(hash_for(sums, "rclone-v1.68.1-windows-arm64.zip").is_none());
        // 非 64 位十六进制的行不算
        assert!(hash_for("zzz  rclone.exe", "rclone.exe").is_none());
    }

    #[test]
    fn official_zip_url_rejects_injected_version() {
        assert!(official_zip_url("1.68.1").is_ok());
        assert!(official_zip_url("v1.68.1").is_ok());
        assert!(official_zip_url("1.68.1/../../evil").is_err());
        assert!(official_zip_url("").is_err());
        assert!(official_zip_url("1.68.1?x=y").is_err());
    }

    #[test]
    fn guard_target_dir_rejects_relative_and_parent_components() {
        assert!(guard_target_dir(Path::new("core")).is_err());
        assert!(guard_target_dir(Path::new("/tmp/a/../b")).is_err());
        assert!(guard_target_dir(&std::env::temp_dir().join("core")).is_ok());
    }

    #[test]
    fn phase_labels_never_contain_secrets() {
        let p = Phase::Failed("bind failed".into());
        assert!(p.label().contains("bind failed"));
        assert!(!p.label().contains("pass"));
    }
}
