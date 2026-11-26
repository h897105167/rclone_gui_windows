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
/// 2. 数据目录 `core/` 那份是**用户明确装过的**（`prefer_managed` 为真，见
///    `crate::config::CoreCfg::has_managed_install`）时，它优先于随包那份，**不比
///    版本号**。渠道（增强版 / 原版）是用户选的，选过之后重启、换便携包都不该被
///    随包那份夺回去；而版本号高低根本代表不了渠道——`v1.77.0-extra` 的数字比
///    `v1.76.0` 大，但用户要的是原版。
///    唯一例外：那份跑不起来（被杀软清成空壳、下载被截断）时不硬用，回落到随包那份，
///    由调用方把回落原因说出来——比"有内核却一直启动不了"好查得多。
/// 3. 没有明确装过（老配置、第一次运行）时，两份都在就**比版本取新的**，数字部分
///    相同时数据目录胜出（`v1.76.0-extra` 与 `v1.76.0` 这种同版本不同渠道）。
///
/// `custom` 非空但指向的 exe 不存在时返回 `Err`，**不静默回落到随包/PATH**：
/// 用户明确指定了 fork，悄悄换成官方 exe 只会让依赖它的插件创建远程时失败，
/// 那种"设置存了但不生效"的故障比直接报错难查得多。
/// `prefer_managed` 为真表示数据目录里那份是用户明确装过/选过的（见文档第 2 条）。
pub fn resolve(
    custom: &str,
    exe_dir: &Path,
    data_dir: &Path,
    prefer_managed: bool,
) -> Result<Option<Located>> {
    resolve_with(custom, exe_dir, data_dir, prefer_managed, which)
}

/// 同 [`resolve`]，但最后一档 PATH 查找由调用方给。
///
/// 分开是为了测试：本机 PATH 里可能装着一个真 rclone，那样"什么都没找到"
/// 这条断言永远不成立，而测试不该依赖开发机的 PATH。
pub fn resolve_with(
    custom: &str,
    exe_dir: &Path,
    data_dir: &Path,
    prefer_managed: bool,
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
        let bv = probe_version(&beside).ok();
        let dv = probe_version(&data).ok();
        let pick_data = pick_data_core(bv.as_deref(), dv.as_deref(), prefer_managed);
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

/// 随包 `core/` 与数据目录 `core/` 都在时，用哪一个。
///
/// 参数是两个 exe 各自报出的版本号（探测失败为 `None`）。三条规则按顺序生效：
///
/// 1. `prefer_managed`（用户明确装过数据目录那份）且它探测成功 ⇒ 取数据目录，
///    不比版本。渠道由用户选，随包那份版本再高也不替用户改主意。
/// 2. 否则版本数字部分不同取新的；数字部分相同也取数据目录那份，因为同版本换渠道时
///    版本号一模一样、只有文件内容变了，而下载/安装的落位处就是数据目录。
/// 3. 探测失败的一侧不参与比较：跑不起来的 exe 版本号再高也用不上——数据目录那份
///    损坏时就回落到随包，而不是硬用它。
fn pick_data_core(beside: Option<&str>, data: Option<&str>, prefer_managed: bool) -> bool {
    match (beside, data) {
        (_, Some(_)) if prefer_managed => true,
        (Some(b), Some(d)) => ver_key(d) >= ver_key(b),
        (None, Some(_)) => true,
        (Some(_), None) => false,
        (None, None) => true, // 两个都跑不起来：选我们管理的那个，便于下次更新覆盖
    }
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
    parse_version_line(&text)
}

/// 从官方放出的那一行文本里取出干净的版本号。
///
/// `version.txt` 的内容是 `rclone v1.76.0`（和 `rclone version` 的第一行同形），
/// 不是裸版本号；直接对整行去 `v` 前缀会把 `rclone v1.76.0` 原封不动当成版本。
/// 所以取最后一个词，再去掉 `v`。
pub fn parse_version_line(text: &str) -> Result<String> {
    let token = text
        .split_whitespace()
        .last()
        .ok_or_else(|| anyhow!("官方版本号格式异常：{text:?}（整行是空的）"))?;
    let v = token.trim().trim_start_matches('v').to_string();
    if v.is_empty()
        || !v
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
    {
        bail!("官方版本号格式异常：{text:?}");
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

// ---------------------------------------------------------------- 发行版渠道

/// 内核下载渠道。
///
/// 两个渠道装的是同一个 `rclone.exe`，区别只在「谁打的包、从哪拿、拿哪个校验值」：
///
/// - [`Flavor::Extra`]：`gulp79/rclone-extra`，在原版之上补了 `teldrive` 等分支后端；
/// - [`Flavor::Official`]：`rclone/rclone` 官方构建，只从 `downloads.rclone.org` 取。
///
/// 渠道只影响下载与更新，**不影响**用户填在 `core.path` 里的那份内核：那份永远优先，
/// 这里不会替换它（查找顺序见 [`crate::config::CoreCfg`] 的文档）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Extra,
    Official,
}

impl Flavor {
    /// 配置里存的是字符串，认不出来的值一律当增强版——它是默认渠道。
    pub fn parse(s: &str) -> Self {
        if s.trim().eq_ignore_ascii_case("official") {
            Flavor::Official
        } else {
            Flavor::Extra
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Flavor::Extra => "extra",
            Flavor::Official => "official",
        }
    }

    /// 界面与日志里的一句话。带上仓库名，用户能自己去看那个 release。
    pub fn label(self) -> &'static str {
        match self {
            Flavor::Extra => "增强版（gulp79/rclone-extra）",
            Flavor::Official => "原版（rclone/rclone）",
        }
    }

    /// 从内核自报的版本串反推渠道。
    ///
    /// 增强版的 tag 带 `-extra` 后缀（例如 `v1.76.0-extra`），它的 `rclone version`
    /// 第一行也带这个后缀；官方版只有数字。这只影响「本机装的是哪一版」的提示，
    /// 下载地址一律由配置决定，不从版本串猜。
    pub fn of_version(version: &str) -> Self {
        if version.to_ascii_lowercase().contains("extra") {
            Flavor::Extra
        } else {
            Flavor::Official
        }
    }
}

/// 增强版 release 的仓库与 Windows x64 包名。
const EXTRA_REPO: &str = "gulp79/rclone-extra";
const WINDOWS_ASSET: &str = "rclone-windows-amd64.zip";

/// 一次「最新版本」查询的结果。字段全部来自发布方，不自己拼。
#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    pub flavor: Flavor,
    /// 去掉 `v` 前缀与 `-extra` 后缀的版本号，可直接交给 [`official_zip_url`]
    pub version: String,
    /// 发布方的 tag，界面原样显示，避免和 GitHub 页面对不上
    pub tag: String,
    /// 文件名；官方渠道用它从 `SHA256SUMS` 里挑行
    pub asset: String,
    /// 发布方报的字节数；0 表示没报
    pub size: u64,
    /// 期望的 sha256，小写 64 位十六进制
    pub sha256: String,
    pub url: String,
    pub published_at: String,
    /// 发布说明的第一行（截断过），只用于界面上给人看
    pub notes: String,
}

impl ReleaseInfo {
    /// 给人看的体积，例如 `28.6 MB`；没报体积时返回空串，让界面自己决定显示什么。
    pub fn size_text(&self) -> String {
        if self.size == 0 {
            String::new()
        } else {
            format!("{:.1} MB", self.size as f64 / (1024.0 * 1024.0))
        }
    }
}

/// `v1.76.0-extra` → `1.76.0`。只接受干净的版本 tag，别的直接报错（不猜）。
pub fn version_of_tag(tag: &str) -> Result<String> {
    let t = tag.trim();
    let t = t
        .strip_prefix('v')
        .or_else(|| t.strip_prefix('V'))
        .unwrap_or(t);
    let mut out = t.to_string();
    if out.to_ascii_lowercase().ends_with("-extra") {
        out.truncate(out.len() - "-extra".len());
    }
    let ok = !out.is_empty()
        && out.chars().all(|c| c.is_ascii_digit() || c == '.')
        && out.split('.').count() >= 2;
    if !ok {
        bail!("版本 tag 不合法：{tag:?}");
    }
    Ok(out)
}

/// 是否是 64 位十六进制 sha256。发布方偶尔给短值或带前缀，那种一律当无效。
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// `candidate` 是否比 `installed` 新。版本串只比数字段，后缀不参与（`1.76.0-extra`
/// 与 `1.76.0` 视为同版本）。
pub fn newer_than(candidate: &str, installed: &str) -> bool {
    ver_key(candidate) > ver_key(installed)
}

/// 已经装着的那个内核，是否还要为 `want` 这个渠道重下一份。
///
/// 只比版本号会漏掉「换渠道」：`1.76.0` 与 `1.76.0-extra` 的数字段相同，但那是两份
/// 不同的二进制（增强版带 fork 专属后端），版本相同也必须重下才算换过去。
/// `recorded` 是上次装好后写进配置的渠道；老配置没写过时按 exe 自报的版本串推。
///
/// `installed` 是实测到的已装版本（探测失败传 `None`），所以坏掉的 exe 一定返回 `true`。
pub fn needs_install(
    want: Flavor,
    latest: &str,
    installed: Option<&str>,
    recorded: Option<Flavor>,
) -> bool {
    let Some(installed) = installed.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    if newer_than(latest, installed) {
        return true;
    }
    if newer_than(installed, latest) {
        // 本机比发布方给的还新（自己编的、或渠道换了更旧的 tag）：不为了对齐而降级。
        return false;
    }
    recorded.unwrap_or_else(|| Flavor::of_version(installed)) != want
}

/// GitHub API 要求带 `User-Agent`，否则 403；顺手把限流说清楚，别让人以为是网络坏了。
async fn fetch_github(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(format!("rcgui/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let hint = if status.as_u16() == 403 || status.as_u16() == 429 {
            "（可能是 GitHub 未登录限流，稍后再试）"
        } else {
            ""
        };
        bail!("{url} 返回 HTTP {status}{hint}");
    }
    Ok(resp.text().await?)
}

/// 把 GitHub `releases/latest` 的 JSON 变成安装计划。
///
/// 只认固定文件名的 Windows x64 包，并且**必须**有发布方给出的
/// `digest: "sha256:..."`。没有校验值的资产一律不装——宁可不更新，也不装一份
/// 无法验证的二进制。
pub fn release_from_github(v: &serde_json::Value, flavor: Flavor) -> Result<ReleaseInfo> {
    if v.get("draft").and_then(|x| x.as_bool()).unwrap_or(false) {
        bail!("最新 release 还是草稿，不自动安装");
    }
    if v.get("prerelease")
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
    {
        bail!("最新 release 是预发布版，不自动安装");
    }
    let tag = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if tag.is_empty() {
        bail!("release 里没有 tag_name");
    }
    let version = version_of_tag(&tag)?;
    let assets = v
        .get("assets")
        .and_then(|x| x.as_array())
        .ok_or_else(|| anyhow!("{tag} 里没有资产列表"))?;
    let asset = assets
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(|x| x.as_str())
                .map(|n| n.eq_ignore_ascii_case(WINDOWS_ASSET))
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("{tag} 里没有 {WINDOWS_ASSET}，无法安装"))?;
    let name = asset
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or(WINDOWS_ASSET)
        .to_string();
    let size = asset.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
    if size > MAX_DOWNLOAD {
        bail!("{name} 有 {size} 字节，超过上限 {MAX_DOWNLOAD}");
    }
    let sha256 = asset
        .get("digest")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .strip_prefix("sha256:")
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| is_sha256_hex(s))
        .ok_or_else(|| {
            anyhow!("{name} 没有发布 sha256 校验值（digest 缺失或格式不对），拒绝安装未校验的内核")
        })?;
    let url = asset
        .get("browser_download_url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if url.is_empty() {
        bail!("{name} 没有下载地址");
    }
    let notes = v
        .get("body")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(160)
        .collect::<String>();
    Ok(ReleaseInfo {
        flavor,
        version,
        tag,
        asset: name,
        size,
        sha256,
        url,
        published_at: v
            .get("published_at")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        notes,
    })
}

/// 查增强版最新 release（GitHub API，未登录每小时 60 次，够用手动检查）。
pub async fn extra_release() -> Result<ReleaseInfo> {
    let url = format!("https://api.github.com/repos/{EXTRA_REPO}/releases/latest");
    let body = fetch_github(&url).await?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("{url} 返回的不是 JSON（可能被代理或限流页替换了）"))?;
    release_from_github(&v, Flavor::Extra)
}

/// 查官方最新版本（`version.txt` + 该版本的 `SHA256SUMS`）。
pub async fn official_release(version: Option<&str>) -> Result<ReleaseInfo> {
    let ver = match version {
        Some(v) if !v.trim().is_empty() => v.trim().trim_start_matches('v').to_string(),
        _ => latest_official_version().await?,
    };
    let zip_url = official_zip_url(&ver)?;
    let zip_name = format!("rclone-v{ver}-windows-amd64.zip");
    let sums_url = format!("https://downloads.rclone.org/v{ver}/SHA256SUMS");
    let sums = fetch_text(&sums_url).await?;
    let sha256 = hash_for(&sums, &zip_name)
        .ok_or_else(|| anyhow!("{sums_url} 里没有 {zip_name} 的校验值"))?;
    Ok(ReleaseInfo {
        flavor: Flavor::Official,
        version: ver.clone(),
        tag: format!("v{ver}"),
        asset: zip_name,
        size: 0,
        sha256,
        url: zip_url,
        published_at: String::new(),
        notes: String::new(),
    })
}

/// 按渠道查最新版本。
pub async fn release_info(flavor: Flavor) -> Result<ReleaseInfo> {
    match flavor {
        Flavor::Extra => extra_release().await,
        Flavor::Official => official_release(None).await,
    }
}

/// 按查到的元数据下载、校验并安装。
///
/// 校验失败时一个字节都不会落到 `target_dir`（先比哈希，再解压）；解压之后的
/// 替换是原子的，详见 [`install_from_zip`]。
pub async fn install_release(target_dir: &Path, info: &ReleaseInfo) -> Result<PathBuf> {
    let bytes = fetch_bytes(&info.url, MAX_DOWNLOAD).await?;
    let got = sha256_hex(&bytes);
    if !got.eq_ignore_ascii_case(&info.sha256) {
        bail!(
            "内核包校验失败：期望 {}，实际 {got}（下载可能被篡改或损坏）",
            info.sha256
        );
    }
    log_line(&format!(
        "{} {} 校验通过（{} 字节）",
        info.flavor.as_str(),
        info.tag,
        bytes.len()
    ));
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

    let (dst, leftover) = swap_core_in(&exe, &stage, target_dir)?;
    // 有进程正从旧文件运行时 Windows 允许改名但不允许删除，备份会留下。
    // 它不影响新文件生效，但占几十 MB，要让人知道而不是悄悄留着。
    if leftover {
        log_line(&format!(
            "旧内核正被运行中的进程占用，备份暂时留在 {}（下次安装会清掉）",
            bak_path(target_dir).display()
        ));
    }
    log_line(&format!("内核已安装到 {}（{ver}）", dst.display()));
    Ok(dst)
}

fn bak_path(target_dir: &Path) -> PathBuf {
    target_dir.join(format!("{CORE_EXE}.bak"))
}

/// 把已经解到 `stage` 的内核换到 `target_dir` 的正式位置。
///
/// 返回落地的路径，以及「旧文件是不是还没删掉」——有人正从它运行时删不掉。
/// 任何一步失败都把旧文件放回原位再返回错误：宁可没换成，也不能让目录里没有内核。
fn swap_core_in(exe: &Path, stage: &Path, target_dir: &Path) -> Result<(PathBuf, bool)> {
    let dst = target_dir.join(CORE_EXE);
    let bak = bak_path(target_dir);
    let _ = std::fs::remove_file(&bak);
    // 旧文件先挪到一边，替换失败才能退回去。正常情况下退到 .bak；而 .bak 这个名字
    // 本身可能被上一轮仍在运行的内核占着（改名可以、删被映射的文件不行），
    // 那就退到暂存目录里，回滚效果一样。
    let mut rollback: Option<PathBuf> = None;
    if dst.exists() {
        if std::fs::rename(&dst, &bak).is_ok() {
            rollback = Some(bak.clone());
        } else {
            let alt = stage.join(format!("{CORE_EXE}.old"));
            if let Err(e) = std::fs::rename(&dst, &alt) {
                let _ = std::fs::remove_dir_all(stage);
                return Err(anyhow!("无法备份现有内核（可能正在被使用）：{e}"));
            }
            rollback = Some(alt);
        }
    }
    if let Err(e) = std::fs::rename(exe, &dst) {
        if let Some(prev) = &rollback {
            let _ = std::fs::rename(prev, &dst);
        }
        let _ = std::fs::remove_dir_all(stage);
        return Err(anyhow!("替换内核失败：{e}"));
    }
    // 暂存目录里除了解出来的文件，还可能躺着上面那份旧内核，一起收掉。
    let _ = std::fs::remove_dir_all(stage);
    let _ = std::fs::remove_file(&bak);
    Ok((dst, bak.exists()))
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

    // ---- 换内核：旧文件先备份，删不掉时要如实报告而不是失败 ----

    #[cfg(windows)]
    #[test]
    fn swap_core_in_puts_the_new_file_in_place_and_drops_the_backup() {
        let dir = tmpdir("swap-plain");
        let stage = dir.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(dir.join(CORE_EXE), b"old").unwrap();
        std::fs::write(stage.join(CORE_EXE), b"new").unwrap();

        let (dst, leftover) = swap_core_in(&stage.join(CORE_EXE), &stage, &dir).unwrap();

        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
        assert!(!leftover, "没人占用时备份应该删干净");
        assert!(!bak_path(&dir).exists());
        assert!(!stage.exists(), "暂存目录要收掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 模拟「上一个内核还在跑、它的映像已被改名成 .bak」：那个名字删不掉也改不了。
    #[cfg(windows)]
    #[test]
    fn swap_core_in_replaces_even_when_the_backup_name_is_taken() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tmpdir("swap-taken");
        let stage = dir.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(dir.join(CORE_EXE), b"old").unwrap();
        std::fs::write(stage.join(CORE_EXE), b"new").unwrap();
        std::fs::write(bak_path(&dir), b"held").unwrap();
        // share_mode(0)：别人既删不掉也改不了名，和映射中的可执行文件一样。
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(bak_path(&dir))
            .unwrap();

        let (dst, leftover) = swap_core_in(&stage.join(CORE_EXE), &stage, &dir).unwrap();

        assert_eq!(
            std::fs::read(&dst).unwrap(),
            b"new",
            "备份名被占也要换上新的"
        );
        assert!(leftover, "删不掉就要说，不能假装干净");
        assert!(!stage.exists(), "走暂存备份路线也要收掉暂存目录");
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 发行版渠道：解析真实 GitHub 元数据的规则 ----

    /// 64 个同字符的十六进制串，当占位哈希用。
    fn hex64(c: char) -> String {
        std::iter::repeat(c).take(64).collect()
    }

    /// 一份结构齐全的 `releases/latest` JSON，资产部分由调用方给。
    fn gh_release(tag: &str, assets: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "published_at": "2026-11-06T20:06:23Z",
            "body": "Update to rclone v1.76.0\n\nEnglish body",
            "assets": assets,
        })
    }

    /// 只有 windows 包一项的资产数组。
    fn win_asset(digest: serde_json::Value, size: u64) -> serde_json::Value {
        serde_json::json!([{
            "name": WINDOWS_ASSET,
            "size": size,
            "digest": digest,
            "browser_download_url": "https://example.invalid/rclone-windows-amd64.zip",
        }])
    }

    #[test]
    fn flavor_only_official_is_official() {
        // 除了明确的 official，其余都按默认的增强版处理：宁可让人装到增强版上，
        // 也不要因为拼错一个词就悄悄换回原版。
        assert_eq!(Flavor::parse("official"), Flavor::Official);
        assert_eq!(Flavor::parse("Official"), Flavor::Official);
        assert_eq!(Flavor::parse("extra"), Flavor::Extra);
        assert_eq!(Flavor::parse(""), Flavor::Extra);
        assert_eq!(Flavor::parse("不认识"), Flavor::Extra);
        assert_eq!(Flavor::Extra.as_str(), "extra");
        assert_eq!(Flavor::Official.as_str(), "official");
        assert!(Flavor::Extra.label().contains(EXTRA_REPO));
        assert!(Flavor::Official.label().contains("rclone/rclone"));
    }

    #[test]
    fn flavor_of_version_reads_the_fork_suffix() {
        assert_eq!(Flavor::of_version("1.76.0-extra"), Flavor::Extra);
        assert_eq!(Flavor::of_version("v1.70.1-Extra"), Flavor::Extra);
        assert_eq!(Flavor::of_version("1.70.1"), Flavor::Official);
        assert_eq!(Flavor::of_version(""), Flavor::Official);
    }

    #[test]
    fn version_of_tag_strips_prefix_and_fork_suffix() {
        assert_eq!(version_of_tag("v1.76.0-extra").unwrap(), "1.76.0");
        assert_eq!(version_of_tag("1.76.0").unwrap(), "1.76.0");
        assert_eq!(version_of_tag(" V1.76.0-EXTRA ").unwrap(), "1.76.0");
    }

    #[test]
    fn version_of_tag_refuses_what_it_would_have_to_guess() {
        assert!(version_of_tag("").is_err());
        assert!(version_of_tag("v1").is_err()); // 只有一段，不像版本号
        assert!(version_of_tag("latest").is_err());
        assert!(version_of_tag("v1.76.0-rc1").is_err()); // 后缀不是 -extra
    }

    #[test]
    fn version_line_from_the_official_site_parses() {
        // downloads.rclone.org/version.txt 真的长这样：整行是 `rclone version` 的第一行。
        assert_eq!(parse_version_line("rclone v1.76.0\n").unwrap(), "1.76.0");
        assert_eq!(parse_version_line("v1.76.0").unwrap(), "1.76.0");
        assert_eq!(parse_version_line("1.76.0").unwrap(), "1.76.0");
        assert!(parse_version_line("").is_err());
        assert!(parse_version_line("   \n").is_err());
        assert!(parse_version_line("latest").is_err());
        assert!(parse_version_line("rclone latest").is_err());
    }

    #[test]
    fn sha256_hex_must_be_64_hex_digits() {
        assert!(is_sha256_hex(&hex64('a')));
        assert!(is_sha256_hex(&hex64('A')));
        assert!(!is_sha256_hex(&hex64('a')[..63]));
        assert!(!is_sha256_hex(&format!("sha256:{}", hex64('a'))));
        assert!(!is_sha256_hex(&format!("{}z", &hex64('a')[..63])));
        assert!(!is_sha256_hex(""));
    }

    #[test]
    fn newer_than_ignores_the_fork_suffix() {
        assert!(newer_than("1.77.0", "1.76.0-extra"));
        assert!(newer_than("1.76.1", "1.76.0"));
        // 同版本、不同渠道：不算「有新版本」，否则切渠道会天天提示更新。
        assert!(!newer_than("1.76.0-extra", "1.76.0"));
        assert!(!newer_than("1.70.1", "1.76.0-extra"));
    }

    #[test]
    fn github_release_becomes_an_install_plan() {
        let v = gh_release(
            "v1.76.0-extra",
            win_asset(
                serde_json::json!(format!("sha256:{}", hex64('a').to_ascii_uppercase())),
                29973118,
            ),
        );
        let info = release_from_github(&v, Flavor::Extra).unwrap();
        assert_eq!(info.flavor, Flavor::Extra);
        assert_eq!(info.version, "1.76.0");
        assert_eq!(info.tag, "v1.76.0-extra");
        assert_eq!(info.asset, WINDOWS_ASSET);
        assert_eq!(info.size, 29973118);
        // 发布方给的是大写也统一成小写，后面比哈希只比一种写法。
        assert_eq!(info.sha256, hex64('a'));
        assert_eq!(info.notes, "Update to rclone v1.76.0");
        assert_eq!(info.published_at, "2026-11-06T20:06:23Z");
        assert!(info.size_text().ends_with("MB"));
    }

    #[test]
    fn github_release_without_digest_is_refused() {
        let v = gh_release("v1.76.0-extra", win_asset(serde_json::Value::Null, 1000));
        let e = release_from_github(&v, Flavor::Extra)
            .unwrap_err()
            .to_string();
        assert!(e.contains("digest"), "{e}");
    }

    #[test]
    fn github_release_with_short_digest_is_refused() {
        let v = gh_release(
            "v1.76.0-extra",
            win_asset(serde_json::json!("sha256:abc"), 1000),
        );
        assert!(release_from_github(&v, Flavor::Extra).is_err());
    }

    #[test]
    fn github_prerelease_is_refused() {
        let mut v = gh_release(
            "v1.76.0-extra",
            win_asset(serde_json::json!(format!("sha256:{}", hex64('b'))), 1000),
        );
        v["prerelease"] = serde_json::json!(true);
        let e = release_from_github(&v, Flavor::Extra)
            .unwrap_err()
            .to_string();
        assert!(e.contains("预发布"), "{e}");
    }

    #[test]
    fn github_oversized_asset_is_refused() {
        let v = gh_release(
            "v1.76.0-extra",
            win_asset(
                serde_json::json!(format!("sha256:{}", hex64('b'))),
                MAX_DOWNLOAD + 1,
            ),
        );
        let e = release_from_github(&v, Flavor::Extra)
            .unwrap_err()
            .to_string();
        assert!(e.contains("超过上限"), "{e}");
    }

    #[test]
    fn github_release_without_windows_asset_is_refused() {
        let v = gh_release(
            "v1.76.0-extra",
            serde_json::json!([{ "name": "rclone-linux-amd64.zip", "size": 1 }]),
        );
        let e = release_from_github(&v, Flavor::Extra)
            .unwrap_err()
            .to_string();
        assert!(e.contains(WINDOWS_ASSET), "{e}");
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
        let find = |custom: &str| resolve_with(custom, &exe_dir, &data, false, |_| None);
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
        let err = resolve(root.join("nope").to_str().unwrap(), &exe_dir, &data, false)
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

    /// 同版本不同渠道：数据目录里那份才算数。
    ///
    /// 增强版报 `v1.76.0-extra`、原版报 `v1.76.0`，数字段完全相同。若这里判给随包
    /// 那份，用户在 1.76.0 上换渠道就会"安装成功、进程重启、端口换新，版本号却一直
    /// 是旧渠道"——界面看起来就是重连之后版本没刷新。
    #[test]
    fn same_numeric_version_prefers_the_kernel_in_the_data_dir() {
        assert!(pick_data_core(
            Some("v1.76.0-extra"),
            Some("v1.76.0"),
            false
        ));
        // 换回去（原版 → 增强版）同样要认数据目录那份
        assert!(pick_data_core(
            Some("v1.76.0"),
            Some("v1.76.0-extra"),
            false
        ));
        // 版本真的更新了，才让版本高的那一侧赢
        assert!(pick_data_core(
            Some("v1.76.0-extra"),
            Some("v1.77.0"),
            false
        ));
        assert!(!pick_data_core(
            Some("v1.77.0"),
            Some("v1.76.0-extra"),
            false
        ));
        // 跑不起来的一侧不参与比较
        assert!(pick_data_core(None, Some("v1.76.0"), false));
        assert!(!pick_data_core(Some("v1.76.0"), None, false));
        assert!(pick_data_core(None, None, false));
    }

    /// 用户明确装过的托管内核，跟随包那份版本高低无关。
    ///
    /// 这一例就是评审指出的跨渠道错位：随包的增强版数字更新（`v1.77.0-extra`），
    /// 用户明确装的原版数字更低（`v1.76.0`）；只比版本必然把渠道改回增强版，
    /// 用户那次安装等于白装。
    #[test]
    fn an_explicit_managed_install_beats_any_bundled_kernel() {
        // 装过：数据目录那份数字更低也认它
        assert!(pick_data_core(Some("v1.77.0-extra"), Some("v1.76.0"), true));
        assert!(pick_data_core(Some("v1.76.0-extra"), Some("v1.76.0"), true));
        // 没装过：同样的输入退回比版本
        assert!(!pick_data_core(
            Some("v1.77.0-extra"),
            Some("v1.76.0"),
            false
        ));
        // 装过但那份跑不起来：回落随包，不能死守一个坏 exe
        assert!(!pick_data_core(Some("v1.76.0-extra"), None, true));
        // 只有数据目录那份：照旧选它
        assert!(pick_data_core(None, Some("v1.76.0"), true));
    }

    /// 手填的路径永远压过托首选，就算装过托管内核也一样。
    ///
    /// 否则又成了"设置存在但不生效"：用户为某个后端特地挑的 fork 会被我们装的那份盖住。
    #[test]
    fn custom_path_wins_even_when_a_managed_install_is_recorded() {
        let root = tmpdir("resolve-custom-first");
        let exe_dir = root.join("app");
        let data = root.join("data");
        let custom = root.join("fork").join(CORE_EXE);
        touch(&exe_dir.join("core").join(CORE_EXE));
        touch(&data.join("core").join(CORE_EXE));
        touch(&custom);

        let l = resolve_with(custom.to_str().unwrap(), &exe_dir, &data, true, |_| None)
            .unwrap()
            .unwrap();
        assert_eq!(l.source, Source::Custom);
        assert_eq!(l.exe, custom);

        // 装过托管内核时，指向不存在的路径同样要报错，不能悄悄改用我们装的那份
        let err = resolve_with(
            root.join("nope").to_str().unwrap(),
            &exe_dir,
            &data,
            true,
            |_| None,
        )
        .expect_err("指定路径不存在时必须报错");
        assert!(err.to_string().contains("不存在"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 应用重启后仍选中用户装的那份，不依赖"这次安装刚好发生"。
    ///
    /// 走的是启动时同一条路：配置从磁盘读回来，判断依据只有 `flavor` 这个持久字段，
    /// 所以重启不会把用户选过的渠道丢掉。
    #[test]
    fn a_restart_keeps_using_the_kernel_the_user_installed() {
        let mut cfg = crate::config::CoreCfg::default();
        assert!(!cfg.has_managed_install(), "全新配置还没装过");
        cfg.flavor = "official".to_string();
        cfg.last_version = "rclone v1.76.0".to_string();

        // 存一轮再读回来，模拟写盘后重新启动
        let json = serde_json::to_string(&cfg).unwrap();
        let reloaded: crate::config::CoreCfg = serde_json::from_str(&json).unwrap();
        assert!(reloaded.has_managed_install());
        assert_eq!(reloaded.normalized_flavor(), Flavor::Official);
        assert!(pick_data_core(
            Some("v1.77.0-extra"),
            Some("v1.76.0"),
            reloaded.has_managed_install()
        ));
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

    /// 同版本换渠道：数字段一样，二进制不一样，必须重下才算真的换过去。
    #[test]
    fn needs_install_reinstalls_on_flavor_switch_at_the_same_version() {
        assert!(needs_install(
            Flavor::Official,
            "1.76.0",
            Some("1.76.0-extra"),
            Some(Flavor::Extra)
        ));
        assert!(needs_install(
            Flavor::Extra,
            "1.76.0-extra",
            Some("1.76.0"),
            Some(Flavor::Official)
        ));
        // 老配置没记渠道时按版本串推，结论一样。
        assert!(needs_install(
            Flavor::Official,
            "1.76.0",
            Some("1.76.0-extra"),
            None
        ));
    }

    /// 渠道没变、版本也一样：不重下（30MB 换不来任何东西）。
    #[test]
    fn needs_install_is_false_when_nothing_would_change() {
        assert!(!needs_install(
            Flavor::Extra,
            "1.76.0-extra",
            Some("1.76.0-extra"),
            Some(Flavor::Extra)
        ));
        assert!(!needs_install(
            Flavor::Official,
            "1.76.0",
            Some("1.76.0"),
            Some(Flavor::Official)
        ));
    }

    /// 发布方更新就重下；本机比发布方还新就不为了对齐而降级。
    #[test]
    fn needs_install_follows_versions_but_never_downgrades() {
        assert!(needs_install(
            Flavor::Extra,
            "1.77.0-extra",
            Some("1.76.0-extra"),
            Some(Flavor::Extra)
        ));
        assert!(!needs_install(
            Flavor::Extra,
            "1.70.0-extra",
            Some("1.76.0-extra"),
            Some(Flavor::Extra)
        ));
    }

    /// 探测不到版本（exe 不在、跑不起来、输出不认识）一律当没装好。
    #[test]
    fn needs_install_is_true_without_a_usable_installed_exe() {
        assert!(needs_install(Flavor::Extra, "1.76.0-extra", None, None));
        assert!(needs_install(
            Flavor::Extra,
            "1.76.0-extra",
            Some("   "),
            Some(Flavor::Extra)
        ));
    }

    /// 配置里记过的渠道比版本串更可信：增强版自报的版本号里也可能没有 `-extra`。
    #[test]
    fn needs_install_trusts_the_recorded_flavor_over_the_version_string() {
        // 装的是增强版（配置记着），版本串看起来像原版：不该判成要换成增强版。
        assert!(!needs_install(
            Flavor::Extra,
            "1.76.0-extra",
            Some("1.76.0"),
            Some(Flavor::Extra)
        ));
        // 同样的 exe 想换原版，那就是真的要换。
        assert!(needs_install(
            Flavor::Official,
            "1.76.0",
            Some("1.76.0"),
            Some(Flavor::Extra)
        ));
    }

    #[test]
    fn phase_labels_never_contain_secrets() {
        let p = Phase::Failed("bind failed".into());
        assert!(p.label().contains("bind failed"));
        assert!(!p.label().contains("pass"));
    }
}
