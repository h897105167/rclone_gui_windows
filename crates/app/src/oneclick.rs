//! 内置插件「一键 rclone」：检测 / 下载安装 / 启动本机 rclone（`rclone rcd`）。
//!
//! ## 与仓库约定的关系（**必须读**）
//!
//! `AGENTS.md` §1/§9 与本项目既有实现都写着「RCGUI 不启动、不停止、不重启 rclone 进程」，
//! 目的是让「连接」只连用户显式指定的 RC 端点，绝不隐含拉起进程。
//!
//! 本模块是**用户显式请求**的例外，边界如下：
//!
//! - 只有用户在本插件里勾选「运行时自动启动 rclone」或点「启用 / 重启」时才会 `spawn`；
//! - 只启动**用户自己选择的那个 rclone.exe**（或本插件下载安装的那份），不内嵌、不释放任何二进制；
//! - 「退出时停止 rclone」默认关闭；只停止**本进程拉起并持有句柄**的那个子进程，
//!   绝不去杀用户自己启动的 rclone；
//! - 核心连接路径（设置页 / 仪表盘 / 离线模式）完全不变，仍然「只连 API」。
//!
//! 因此这里刻意不放在 `rclone_client` crate 里：那个 crate 有一条防回归单测
//! （`crate_has_no_process_spawning`）明确要求它不含任何进程管理代码。

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------- 配置（持久化到 config.json 的 oneclick 段） ----------------

fn d_port() -> u16 {
    5572
}
fn d_true() -> bool {
    true
}

/// 「一键 rclone」插件的持久化配置。
///
/// 注意：`rc_pass` 是**明文**存在 `%APPDATA%\RCGUI\config.json`，与 `rclone.pass`
/// 同一笔既有安全债（见 WORKFLOW 第四节第 5 条）。任何输出/日志/文档都不得回显它。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OneClickCfg {
    /// 用户选定 / 本插件安装的 rclone.exe 绝对路径（空 = 未设置）
    #[serde(default)]
    pub exe: String,
    /// 最近一次校验读到的版本（展示用，如 `v1.75.4`）
    #[serde(default)]
    pub exe_version: String,
    /// 0 官方版 / 1 增强版（gulp79/rclone-extra）
    #[serde(default)]
    pub edition: u8,
    /// 0 GitHub / 官网 / 1 清华镜像源（含国内镜像回退）
    #[serde(default)]
    pub source: u8,
    /// `--rc-user`；空 = 不携带该参数
    #[serde(default)]
    pub rc_user: String,
    /// `--rc-pass`；空 = 不携带该参数
    #[serde(default)]
    pub rc_pass: String,
    /// `--rc-addr` 的端口（只监听 127.0.0.1）
    #[serde(default = "d_port")]
    pub port: u16,
    /// 勾选后加 `--rc-no-auth`，并**不**携带 `--rc-user` / `--rc-pass`
    #[serde(default)]
    pub no_auth: bool,
    /// 静默启动（隐藏终端窗口）
    #[serde(default = "d_true")]
    pub silent: bool,
    /// 运行时自动启动 rclone（要求 `exe` 已设置并通过校验）
    #[serde(default)]
    pub auto_start: bool,
    /// 退出 RCGUI 时停止由本程序拉起的 rclone
    #[serde(default)]
    pub stop_on_exit: bool,
    /// 一键安装目录（展示用；真实路径以 `exe` 为准）
    #[serde(default)]
    pub install_dir: String,
}

impl Default for OneClickCfg {
    fn default() -> Self {
        Self {
            exe: String::new(),
            exe_version: String::new(),
            edition: 0,
            source: 0,
            // 默认带一个用户名：`--rc-user` 单独存在时 rclone 会启用鉴权
            // （实测 Basic `gui:` 可访问 config/*；完全不给凭据则 config/* 被拒）
            rc_user: "gui".into(),
            rc_pass: String::new(),
            port: d_port(),
            no_auth: false,
            silent: true,
            auto_start: false,
            stop_on_exit: false,
            install_dir: String::new(),
        }
    }
}

impl OneClickCfg {
    /// 路径是否已设置。
    pub fn has_exe(&self) -> bool {
        !self.exe.trim().is_empty()
    }

    /// 是否允许勾选「运行时自动启动 rclone」。
    ///
    /// 需求约束：**必须先把路径设置完毕**才能动这个开关。UI 上还额外要求
    /// 「校验通过」（`PluginsG.oc-verified`），否则用户选了个假 rclone 也能自启，
    /// 每次开机都失败一次。
    pub fn auto_start_allowed(&self) -> bool {
        self.has_exe()
    }

    /// 启动时会以「无鉴权」运行：没勾 no_auth，但用户名/密码都空。
    ///
    /// 这种情况下 rclone 会拒绝 `config/*`（实测原文：
    /// `authentication must be set up on the rc server to use "config/providers"`），
    /// UI 需要提前告警，而不是等用户看到一堆红字。
    pub fn authless(&self) -> bool {
        !self.no_auth && self.rc_user.trim().is_empty() && self.rc_pass.is_empty()
    }
}

// ---------------- exe 校验 ----------------

/// 校验清单的一行（UI 直接渲染成 ✓ / ✗）。
#[derive(Debug, Clone, PartialEq)]
pub struct CheckRow {
    pub label: String,
    pub value: String,
    pub ok: bool,
}

impl CheckRow {
    fn new(label: &str, value: impl Into<String>, ok: bool) -> Self {
        Self {
            label: label.to_string(),
            value: value.into(),
            ok,
        }
    }
}

/// 校验结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExeCheck {
    pub path: String,
    /// 综合结论：只有「确实是 rclone」才算 true
    pub ok: bool,
    /// 探测到的版本（如 `v1.75.4`），失败为空
    pub version: String,
    /// 失败原因（面向用户，可直接进 toast）
    pub detail: String,
    pub rows: Vec<CheckRow>,
}

/// rclone.exe 的合理体积区间。
///
/// 实测：v1.75.4 windows-amd64 为 84,312,576 字节（约 80 MB）；老版本 v1.50 约 18 MB。
/// 这里给出宽容区间，只用来排除「明显不是可执行文件」的目标（txt / 快捷方式 / 空文件）。
const MIN_EXE_SIZE: u64 = 4 * 1024 * 1024;
const MAX_EXE_SIZE: u64 = 300 * 1024 * 1024;

/// 校验一个候选 rclone.exe。
///
/// 判定链（从弱到强）：
/// 1. 文件存在且是普通文件；
/// 2. 文件名含 `rclone`（**软信号**：官方文件改名后仍然可用，此时只提示）；
/// 3. 体积在合理区间（排除 txt / 空壳）；
/// 4. PE 头有效（排除改名的文本文件）；
/// 5. 数字签名（官方 Windows 构建实测为 **未签名**，因此只作旁证，不作为硬性条件）；
/// 6. **执行 `<exe> version` 并解析 `rclone vX.Y.Z`** —— 唯一的硬性证据。
pub fn validate(path: &str) -> ExeCheck {
    let mut out = ExeCheck {
        path: path.to_string(),
        ..Default::default()
    };
    let raw = path.trim();
    if raw.is_empty() {
        out.rows.push(CheckRow::new("路径", "未设置", false));
        out.detail = "还没有选择 rclone.exe".into();
        return out;
    }
    let p = Path::new(raw);
    let exists = p.is_file();
    out.rows.push(CheckRow::new(
        "文件",
        if exists {
            raw.to_string()
        } else {
            format!("{raw}（不存在）")
        },
        exists,
    ));
    if !exists {
        out.detail = format!("文件不存在或不是普通文件：{raw}");
        return out;
    }

    // ---- 文件名（软信号）----
    let fname = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let name_ok = fname.to_ascii_lowercase().contains("rclone");
    out.rows.push(CheckRow::new(
        "文件名",
        if name_ok {
            fname.clone()
        } else {
            format!("{fname}（不含 rclone，将按版本探测确认）")
        },
        name_ok,
    ));

    // ---- 体积 ----
    let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let size_ok = (MIN_EXE_SIZE..=MAX_EXE_SIZE).contains(&size);
    out.rows.push(CheckRow::new(
        "文件大小",
        format!("{:.1} MB", size as f64 / 1024.0 / 1024.0),
        size_ok,
    ));

    // ---- PE 头 ----
    let pe = pe_info(p);
    let pe_ok = pe.is_ok();
    out.rows.push(CheckRow::new(
        "可执行格式",
        match &pe {
            Ok(s) => s.clone(),
            Err(e) => format!("非有效 PE 文件（{e}）"),
        },
        pe_ok,
    ));

    // ---- 数字签名（旁证；官方 Windows 构建通常未签名）----
    let (sig_text, sig_ok) = signature_of(p);
    out.rows.push(CheckRow::new("数字签名", sig_text, sig_ok));

    // ---- 版本探测（硬性证据）----
    //
    // 只有「格式与体积都像可执行文件」时才真的执行它：校验过程会 launch 这个文件，
    // 用户明确选择了一个 exe 是预期行为，但不该为一个 .txt 或 200 字节的壳子去 launch。
    let probe = if pe_ok && size_ok {
        probe_version(p)
    } else {
        Err(anyhow!("已跳过（文件格式或体积不合格，不执行未知文件）"))
    };
    let (ver, probe_text, probe_ok) = match &probe {
        Ok(v) => (
            v.version.clone(),
            format!("{} / {}/{}", v.headline, v.os_type, v.arch),
            true,
        ),
        Err(e) => (String::new(), format!("{e}"), false),
    };
    out.rows.push(CheckRow::new("版本探测", probe_text, probe_ok));
    out.version = ver.clone();

    // 综合结论：格式 + 体积 + 版本探测（文件名与签名只影响提示）
    out.ok = pe_ok && size_ok && probe_ok;
    if !out.ok {
        out.detail = if !size_ok {
            format!(
                "文件大小异常（{:.1} MB），不像是 rclone.exe",
                size as f64 / 1024.0 / 1024.0
            )
        } else if !pe_ok {
            format!("不是 Windows 可执行文件（PE 头无效）：{raw}")
        } else {
            match probe {
                Err(e) => format!("`rclone version` 失败：{e}。该文件不是 rclone，或已损坏。"),
                Ok(_) => "校验未通过".into(),
            }
        };
    }
    out
}

/// 读取 PE 头，返回「PE32+ amd64 / 控制台」这类描述。
fn pe_info(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p).with_context(|| "打开文件失败")?;
    let mut head = [0u8; 64];
    let mut read = 0usize;
    while read < head.len() {
        let n = f.read(&mut head[read..]).with_context(|| "读取文件头失败")?;
        if n == 0 {
            break;
        }
        read += n;
    }
    if read < 64 || &head[0..2] != b"MZ" {
        bail!("缺少 MZ 头");
    }
    let e_lfanew = u32::from_le_bytes([head[0x3C], head[0x3D], head[0x3E], head[0x3F]]) as u64;
    if e_lfanew < 64 || e_lfanew > 64 * 1024 * 1024 {
        bail!("PE 头偏移异常");
    }
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(e_lfanew))?;
    let mut sig = [0u8; 4];
    f.read_exact(&mut sig).with_context(|| "读取 PE 签名失败")?;
    if &sig != b"PE\0\0" {
        bail!("缺少 PE 签名");
    }
    // COFF 头 20 字节：machine 在 +0，之后需要 optional header；
    // PE32/PE32+ 的 Subsystem 都在 optional header 偏移 68。
    let mut coff = [0u8; 20];
    f.read_exact(&mut coff)?;
    let machine = u16::from_le_bytes([coff[0], coff[1]]);
    let mut opt_head = [0u8; 2];
    f.read_exact(&mut opt_head)?;
    let magic = u16::from_le_bytes(opt_head);
    f.seek(std::io::SeekFrom::Start(e_lfanew + 24 + 68))?;
    let mut sub = [0u8; 2];
    f.read_exact(&mut sub)?;
    let subsystem = u16::from_le_bytes(sub);
    let arch = match machine {
        0x8664 => "amd64",
        0x014c => "x86",
        0xAA64 => "arm64",
        0x01c0 => "arm",
        _ => "未知架构",
    };
    let bits = match magic {
        0x20b => "PE32+",
        0x10b => "PE32",
        _ => "PE?",
    };
    let kind = match subsystem {
        2 => "GUI 程序",
        3 => "控制台程序",
        _ => "其它子系统",
    };
    Ok(format!("{bits} / {arch} / {kind}"))
}

/// 数字签名：用 `Get-AuthenticodeSignature` 读。
///
/// - `Valid` 且发布者含 rclone → 强旁证（ok=true）
/// - `Valid` 但发布者不是 rclone → 可能是第三方重打包（ok=false，提示但不阻断）
/// - `NotSigned` → 官方 Windows 构建实测就是未签名（ok=true，避免误报）
/// - 读不到（系统裁剪了 PowerShell / 超时）→ ok=false + 原文，仍不阻断判定
fn signature_of(p: &Path) -> (String, bool) {
    let quoted = p.display().to_string().replace('\'', "''");
    let script = format!(
        "$s=Get-AuthenticodeSignature -LiteralPath '{quoted}'; \
         if($s.Status -eq 'Valid'){{'VALID'}}else{{$s.Status.ToString()}}; \
         if($s.SignerCertificate){{$s.SignerCertificate.Subject}}"
    );
    let mut cmd = std::process::Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        script.as_str(),
    ]);
    let out = match run_capture(cmd, Duration::from_secs(8)) {
        Ok(o) => o,
        Err(e) => return (format!("无法读取（{e}）"), false),
    };
    let mut lines = out.lines().map(|l| l.trim()).filter(|l| !l.is_empty());
    let status = lines.next().unwrap_or("").to_string();
    let subject = lines.next().unwrap_or("").to_string();
    if status.eq_ignore_ascii_case("VALID") {
        if subject.to_ascii_lowercase().contains("rclone") {
            (format!("有效：{subject}"), true)
        } else {
            (
                format!("有效但不是 rclone 发布者：{subject}"),
                false,
            )
        }
    } else if status.eq_ignore_ascii_case("NotSigned") {
        ("未签名（rclone 官方 Windows 构建通常未签名）".into(), true)
    } else if status.is_empty() {
        ("无法读取（无输出）".into(), false)
    } else {
        (format!("状态 {status}"), false)
    }
}

/// `rclone version` 的输出摘要。
#[derive(Debug, Clone, PartialEq)]
pub struct VersionProbe {
    /// 首行原文，如 `rclone v1.75.4`
    pub headline: String,
    /// 解析出的版本号，如 `v1.75.4`
    pub version: String,
    pub os_type: String,
    pub arch: String,
}

/// 执行 `<exe> version` 并解析。这是「到底是不是 rclone」的唯一硬证据。
pub fn probe_version(p: &Path) -> Result<VersionProbe> {
    let mut cmd = std::process::Command::new(p);
    cmd.arg("version");
    let out = run_capture(cmd, Duration::from_secs(15))
        .with_context(|| "无法执行该文件（可能不是可执行文件，或被安全软件拦截）")?;
    let mut headline = String::new();
    let mut os_type = String::new();
    let mut arch = String::new();
    let mut version = String::new();
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if headline.is_empty() && t.to_ascii_lowercase().starts_with("rclone v") {
            headline = t.to_string();
            version = t
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .trim()
                .to_string();
            continue;
        }
        if let Some(rest) = t.strip_prefix("- os/type:") {
            os_type = rest.trim().to_string();
        } else if let Some(rest) = t.strip_prefix("- os/arch:") {
            arch = rest.trim().to_string();
        }
    }
    if version.is_empty() {
        let first: String = out.lines().take(3).collect::<Vec<_>>().join(" / ");
        bail!("输出里没有 `rclone vX.Y.Z`（实际: {first}）");
    }
    if !os_type.is_empty() && !os_type.eq_ignore_ascii_case("windows") {
        bail!("这是 {os_type} 平台的构建，不能在 Windows 上运行");
    }
    Ok(VersionProbe {
        headline,
        version,
        os_type: if os_type.is_empty() {
            "windows".into()
        } else {
            os_type
        },
        arch: if arch.is_empty() { "?".into() } else { arch },
    })
}

/// 启动子进程并收集输出（带超时）。
///
/// 实现上的取舍：用 `try_wait` 轮询 + `wait_with_output` 收尾，而不是
/// 「线程 + recv_timeout」。代价是**子进程输出超过管道缓冲（64 KB）时会卡住**——
/// 这里只跑 `version` 和一个 PowerShell 单行命令，输出都在几百字节级，不会触发。
fn run_capture(mut cmd: std::process::Command, timeout: Duration) -> Result<String> {
    use std::process::Stdio;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(&mut cmd);
    let mut child = cmd.spawn().with_context(|| "无法启动子进程")?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("执行超时（{}s）", timeout.as_secs());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => bail!("等待子进程失败: {e}"),
        }
    }
    let out = child.wait_with_output().with_context(|| "读取子进程输出失败")?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.stderr.is_empty() {
        text.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    Ok(text)
}

// ---------------- 启动参数与进程管理 ----------------

/// 一次启动的完整计划（纯数据，便于单测）。
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub silent: bool,
}

/// 构造 `rclone rcd` 的启动参数。
///
/// 规则（对应用户需求里的参数 3/4/5/6/7）：
/// - 端口 → `--rc-addr 127.0.0.1:<port>`（**只绑回环**，不对外暴露）
/// - `no_auth` → 只加 `--rc-no-auth`，**不**带 `--rc-user` / `--rc-pass`
/// - 用户名非空 → `--rc-user <user>`；密码非空 → `--rc-pass <pass>`（各自独立判空）
/// - `silent` → 启动时用 `CREATE_NO_WINDOW` 隐藏终端（等价于 PowerShell 的
///   `-WindowStyle Hidden`，但不经过 PowerShell，没有引号/编码风险）
pub fn launch_plan(cfg: &OneClickCfg) -> Result<LaunchPlan> {
    let exe = cfg.exe.trim();
    if exe.is_empty() {
        bail!("还没有设置 rclone.exe 路径");
    }
    if !Path::new(exe).is_file() {
        bail!("rclone.exe 不存在：{exe}");
    }
    if cfg.port == 0 {
        bail!("端口不能为 0");
    }
    let mut args = vec![
        "rcd".to_string(),
        "--rc-addr".to_string(),
        format!("127.0.0.1:{}", cfg.port),
    ];
    if cfg.no_auth {
        args.push("--rc-no-auth".to_string());
    } else {
        let u = cfg.rc_user.trim();
        if !u.is_empty() {
            args.push("--rc-user".to_string());
            args.push(u.to_string());
        }
        if !cfg.rc_pass.is_empty() {
            args.push("--rc-pass".to_string());
            args.push(cfg.rc_pass.clone());
        }
    }
    Ok(LaunchPlan {
        program: exe.to_string(),
        args,
        silent: cfg.silent,
    })
}

/// 本程序拉起并持有的那个 rclone 子进程。
struct Owned {
    pid: u32,
    child: std::process::Child,
    started: std::time::Instant,
}

static OWNED: Mutex<Option<Owned>> = Mutex::new(None);

#[cfg(windows)]
fn no_window(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW = 0x08000000
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut std::process::Command) {}

/// 由本程序拉起的 rclone 是否还活着；活着返回 PID。
///
/// 顺带做回收：进程已退出时把句柄清掉（`try_wait` 会 reap，避免僵尸）。
pub fn running_pid() -> Option<u32> {
    let mut g = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    let exited = match g.as_mut() {
        Some(o) => match o.child.try_wait() {
            Ok(None) => return Some(o.pid),
            _ => true,
        },
        None => false,
    };
    if exited {
        *g = None;
    }
    None
}

/// 「一键 rclone」日志文件（仅在静默模式下重定向子进程输出）。
pub fn log_path() -> PathBuf {
    crate::config::data_dir().join("logs").join("oneclick-rclone.log")
}

fn tail_log(path: &Path, max_lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join(" ⏎ ")
}

/// 启动 rclone（`rcd`）。返回 PID。
///
/// 会用 `try_wait` 观察 1.2 秒：端口被占用这类错误 rclone 会立刻退出，
/// 这里就能带上日志末尾几行，而不是只给一个"已启动"的假成功。
pub fn start(plan: &LaunchPlan) -> Result<u32> {
    if let Some(pid) = running_pid() {
        bail!("本程序拉起的 rclone 已在运行（PID {pid}）");
    }
    let mut cmd = std::process::Command::new(&plan.program);
    cmd.args(&plan.args);
    let mut log = None;
    if plan.silent {
        let path = log_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        if let Some(f) = file {
            let f2 = f.try_clone().ok();
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::from(f))
                .stderr(f2.map(std::process::Stdio::from).unwrap_or_else(std::process::Stdio::null));
            log = Some(path);
        } else {
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
        no_window(&mut cmd);
    }
    // 非静默：不重定向、不加 CREATE_NO_WINDOW —— rclone 会拿到自己的控制台窗口，
    // 日志直接打在窗口里（这就是「不隐藏终端」的预期行为）。

    let child = cmd
        .spawn()
        .with_context(|| format!("无法启动 {}", plan.program))?;
    let pid = child.id();
    *OWNED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Owned {
        pid,
        child,
        started: std::time::Instant::now(),
    });

    // 观察 1.2s：正常启动后进程应该还活着
    let deadline = std::time::Instant::now() + Duration::from_millis(1200);
    loop {
        std::thread::sleep(Duration::from_millis(120));
        if running_pid().is_none() {
            let tail = log
                .as_deref()
                .map(|p| tail_log(p, 3))
                .unwrap_or_default();
            let hint = if tail.is_empty() {
                "（没有日志：可能被安全软件拦截，或端口被占用）".to_string()
            } else {
                format!("（日志：{tail}）")
            };
            bail!("rclone 启动后立刻退出{hint}");
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
    }
    Ok(pid)
}

/// 停止本程序拉起的 rclone（只动自己持有的那个 PID，绝不误杀用户自己的实例）。
pub fn stop() -> Result<bool> {
    let taken = {
        let mut g = OWNED.lock().unwrap_or_else(|e| e.into_inner());
        g.take()
    };
    let Some(mut o) = taken else {
        return Ok(false);
    };
    let _ = o.child.kill();
    let _ = o.child.wait();
    // 兜底：rclone 若派生了子进程（理论上 rcd 不会），用 taskkill 按树清理。
    // 失败无所谓（进程已经不存在了）。
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("taskkill.exe");
        cmd.args(["/PID", &o.pid.to_string(), "/T", "/F"]);
        no_window(&mut cmd);
        let _ = cmd.output();
    }
    Ok(true)
}

/// 本程序拉起的 rclone 已经运行了多久（用于 UI 展示）。
pub fn uptime_secs() -> Option<u64> {
    let g = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    g.as_ref().map(|o| o.started.elapsed().as_secs())
}

// ---------------- 端口探测 ----------------

/// 端口是否已有进程在监听（用于区分「端口被占用」和「启动太慢」）。
pub async fn port_open(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_millis(600),
            tokio::net::TcpStream::connect(("127.0.0.1", port))
        )
        .await,
        Ok(Ok(_))
    )
}

/// 等端口就绪（rclone 启动后要过一小会儿才 bind）。
pub async fn wait_port(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if port_open(port).await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ---------------- 下载与安装 ----------------

/// 下载安装过程中的阶段（UI 用它显示进度）。
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    /// 正在解析版本 / 选择下载源
    Resolve(String),
    /// 已确定来源（展示用：来源 + 版本 + URL）
    Source(String),
    /// 下载中（total 为 None 时表示服务器没给 Content-Length）
    Download { done: u64, total: Option<u64> },
    Extract(String),
    Verify(String),
    Install(String),
}

/// 进度回调（跨线程调用，内部只应做 UI 投递）。
pub type Progress = Arc<dyn Fn(Stage) + Send + Sync>;

/// 解析出来的下载目标。
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// 展示用版本，如 `v1.75.2` 或 `v1.75.2-extra`
    pub version: String,
    /// 压缩包下载地址
    pub url: String,
    /// 来源描述（如「官方源」「南京大学镜像」「GitHub Releases」）
    pub label: String,
    /// 附加说明（镜像回退等），逐条展示给用户
    pub notes: Vec<String>,
}

const OFFICIAL_BASE: &str = "https://downloads.rclone.org";
/// 国内镜像候选：清华镜像优先（用户指定），但它目前**没有** rclone 目录
/// （2026-10-18 实测 `/rclone/` `/rclone/version.txt` `/help/rclone/` 全 404），
/// 因此按顺序回退到南京大学镜像（实测有 `version.txt` 与各版本目录）。
const MIRRORS: &[(&str, &str)] = &[
    ("清华镜像", "https://mirrors.tuna.tsinghua.edu.cn/rclone"),
    ("南京大学镜像", "https://mirrors.nju.edu.cn/rclone"),
];
const EXTRA_REPO_API: &str = "https://api.github.com/repos/gulp79/rclone-extra/releases/latest";
const MAX_DOWNLOAD: u64 = 400 * 1024 * 1024;

/// 当前 CPU 架构对应的 rclone 发行包后缀。
pub fn rclone_arch() -> Result<&'static str> {
    Ok(match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "x86" => "386",
        "aarch64" => "arm64",
        other => bail!("不支持的 CPU 架构：{other}"),
    })
}

/// 官方版压缩包文件名：`rclone-v1.75.2-windows-amd64.zip`
pub fn official_zip_name(version: &str, arch: &str) -> String {
    format!("rclone-{version}-windows-{arch}.zip")
}

/// 官方版下载地址：`{base}/{ver}/rclone-{ver}-windows-{arch}.zip`
pub fn official_url(base: &str, version: &str, arch: &str) -> String {
    format!(
        "{}/{}/{}",
        base.trim_end_matches('/'),
        version,
        official_zip_name(version, arch)
    )
}

/// 增强版压缩包文件名：`rclone-windows-amd64.zip`（rclone-extra 的资产命名）
pub fn extra_zip_name(arch: &str) -> String {
    format!("rclone-windows-{arch}.zip")
}

/// 增强版下载地址：`https://github.com/gulp79/rclone-extra/releases/download/{tag}/{asset}`
pub fn extra_url(tag: &str, arch: &str) -> String {
    format!(
        "https://github.com/gulp79/rclone-extra/releases/download/{}/{}",
        tag,
        extra_zip_name(arch)
    )
}

/// 解析 `version.txt`（内容形如 `rclone v1.75.2`）。
pub fn parse_version_txt(s: &str) -> Option<String> {
    let t = s.trim();
    let t = t.strip_prefix("rclone").unwrap_or(t).trim();
    let v = t.split_whitespace().next()?.trim();
    if v.starts_with('v') && v.len() > 1 && v[1..].starts_with(|c: char| c.is_ascii_digit()) {
        Some(v.to_string())
    } else {
        None
    }
}

/// 从 GitHub releases API 的 JSON 里取 tag 与 zip 资产的直链。
pub fn parse_github_release(json: &serde_json::Value, arch: &str) -> Result<(String, Option<String>)> {
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("GitHub 返回里没有 tag_name"))?
        .trim()
        .to_string();
    if tag.is_empty() {
        bail!("GitHub 返回的 tag_name 为空");
    }
    let want = extra_zip_name(arch);
    let mut names: Vec<String> = Vec::new();
    let mut found = None;
    if let Some(list) = json.get("assets").and_then(|a| a.as_array()) {
        for a in list {
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            names.push(name.to_string());
            if name.eq_ignore_ascii_case(&want) {
                found = a
                    .get("browser_download_url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
        }
    }
    if found.is_none() && !names.is_empty() {
        bail!(
            "该版本没有 {want}（现有资产：{}）",
            names.into_iter().take(8).collect::<Vec<_>>().join("、")
        );
    }
    Ok((tag, found))
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .user_agent(format!("RCGUI/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .with_context(|| "创建 HTTP 客户端失败")
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let r = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("请求 {url} 失败"))?;
    let status = r.status();
    if !status.is_success() {
        bail!("{url} 返回 HTTP {}", status.as_u16());
    }
    r.text().await.with_context(|| format!("读取 {url} 响应失败"))
}

/// 解析「装哪个版本、从哪下」。
pub async fn resolve(edition: u8, source: u8) -> Result<Resolved> {
    let arch = rclone_arch()?;
    let client = http_client()?;
    let mut notes = Vec::new();
    if edition == 1 {
        // 增强版只有 GitHub Releases 一个发布渠道，镜像源对它无效
        if source == 1 {
            notes.push("增强版只发布在 GitHub Releases，镜像源对它不可用，已直连 GitHub".to_string());
        }
        let text = fetch_text(&client, EXTRA_REPO_API).await?;
        let json: serde_json::Value =
            serde_json::from_str(&text).with_context(|| "GitHub Releases 响应不是合法 JSON")?;
        let (tag, asset) = parse_github_release(&json, arch)?;
        let url = asset.unwrap_or_else(|| extra_url(&tag, arch));
        return Ok(Resolved {
            version: tag,
            url,
            label: "GitHub Releases（rclone-extra）".into(),
            notes,
        });
    }

    // 官方版
    let mut bases: Vec<(&str, String)> = Vec::new();
    if source == 1 {
        for (name, base) in MIRRORS {
            bases.push((name, (*base).to_string()));
        }
        bases.push(("官方源（镜像不可用，已回退）", OFFICIAL_BASE.to_string()));
    } else {
        bases.push(("官方源", OFFICIAL_BASE.to_string()));
    }
    let mut last_err = String::new();
    for (name, base) in bases {
        let vurl = format!("{}/version.txt", base);
        match fetch_text(&client, &vurl).await {
            Ok(text) => match parse_version_txt(&text) {
                Some(version) => {
                    if name.contains("回退") {
                        notes.push(format!("镜像源不可用，已回退到官方源（{last_err}）"));
                    } else if source == 1 {
                        notes.push(format!("使用{name}"));
                    }
                    return Ok(Resolved {
                        url: official_url(&base, &version, arch),
                        version,
                        label: name.to_string(),
                        notes,
                    });
                }
                None => last_err = format!("{vurl} 内容无法解析"),
            },
            Err(e) => last_err = format!("{e}"),
        }
    }
    bail!("无法解析 rclone 版本：{last_err}")
}

/// 下载并安装 rclone。
///
/// 安装位置：
/// - 已提权：`%ProgramFiles%\RCGUI\rclone\<version>\rclone.exe`
/// - 未提权：`%LOCALAPPDATA%\RCGUI\rclone\<version>\rclone.exe`
///
/// 提权后仍写不进系统目录时（组策略 / ACL）自动回退到用户目录，并在 `notes` 里说明，
/// 而不是让用户卡在一个没有解释的「拒绝访问」。
pub async fn install(
    edition: u8,
    source: u8,
    elevated: bool,
    progress: Progress,
) -> Result<InstallOutcome> {
    progress(Stage::Resolve("正在解析最新版本…".into()));
    let resolved = resolve(edition, source).await?;
    progress(Stage::Source(format!(
        "{} · {} · {}",
        resolved.label, resolved.version, resolved.url
    )));
    for n in &resolved.notes {
        progress(Stage::Resolve(n.clone()));
    }

    let client = http_client()?;
    let bytes = download(&client, &resolved.url, &progress).await?;

    progress(Stage::Extract("正在解压…".into()));
    let tmp = crate::config::data_dir()
        .join("tmp")
        .join(format!("oneclick-{}-{}", std::process::id(), std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)));
    let tmp2 = tmp.clone();
    let data = bytes;
    let files = tokio::task::spawn_blocking(move || -> Result<usize> {
        std::fs::create_dir_all(&tmp2)?;
        plugin_host::zip::extract_all(&data, &tmp2, 600 * 1024 * 1024)
    })
    .await
    .map_err(|e| anyhow!("解压线程异常: {e}"))??;

    let src = find_rclone_exe(&tmp)
        .ok_or_else(|| anyhow!("压缩包里没有 rclone.exe（已解压 {files} 个文件）"))?;

    progress(Stage::Verify("正在校验解压出的 rclone.exe…".into()));
    let src2 = src.clone();
    let check = tokio::task::spawn_blocking(move || validate(&src2.display().to_string()))
        .await
        .map_err(|e| anyhow!("校验线程异常: {e}"))?;
    if !check.ok {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("下载到的文件未通过校验：{}", check.detail);
    }

    progress(Stage::Install("正在安装…".into()));
    let mut notes = resolved.notes.clone();
    notes.extend(mirror_notes(&resolved.url));
    let tag = safe_dir_name(&resolved.version);
    let mut installed_elevated = elevated;
    let mut root = install_root(elevated);
    let mut target = root.join(&tag);
    let mut dst = target.join("rclone.exe");
    if let Err(e) = copy_exe(&src, &dst, &target) {
        if elevated {
            // 已提权却写不进系统目录：回退用户目录（附原因）
            root = per_user_root();
            target = root.join(&tag);
            dst = target.join("rclone.exe");
            copy_exe(&src, &dst, &target).map_err(|e2| {
                anyhow!("安装到系统目录失败（{e}），回退用户目录也失败（{e2}）")
            })?;
            installed_elevated = false;
            notes.push(format!("系统目录不可写（{e}），已改为用户目录"));
        } else {
            return Err(e);
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);

    Ok(InstallOutcome {
        exe: dst.display().to_string(),
        version: if check.version.is_empty() {
            resolved.version.clone()
        } else {
            check.version.clone()
        },
        dir: target.display().to_string(),
        elevated: installed_elevated,
        notes,
    })
}

/// 安装结果。
#[derive(Debug, Clone, PartialEq)]
pub struct InstallOutcome {
    pub exe: String,
    pub version: String,
    pub dir: String,
    /// 是否装进了系统目录（`%ProgramFiles%`）
    pub elevated: bool,
    pub notes: Vec<String>,
}

fn mirror_notes(url: &str) -> Vec<String> {
    if MIRRORS.iter().any(|(_, base)| url.starts_with(base)) {
        vec![format!(
            "下载源不是官方站点（{}），如需严格校验请核对官方 SHA256SUMS",
            url.split('/').nth(2).unwrap_or("镜像")
        )]
    } else {
        Vec::new()
    }
}

async fn download(client: &reqwest::Client, url: &str, progress: &Progress) -> Result<Vec<u8>> {
    let mut r = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("下载 {url} 失败"))?;
    let status = r.status();
    if !status.is_success() {
        bail!("下载失败：{url} 返回 HTTP {}", status.as_u16());
    }
    let total = r.content_length();
    let mut out: Vec<u8> = Vec::with_capacity(total.unwrap_or(0).min(64 * 1024 * 1024) as usize);
    let mut done = 0u64;
    let mut last_report = std::time::Instant::now();
    while let Some(chunk) = r.chunk().await.with_context(|| "下载中断")? {
        done += chunk.len() as u64;
        if done > MAX_DOWNLOAD {
            bail!("下载体积异常（超过 {} MB），已中止", MAX_DOWNLOAD / 1024 / 1024);
        }
        out.extend_from_slice(&chunk);
        // 节流：200ms 一次，避免刷爆 UI 事件队列
        if last_report.elapsed() >= Duration::from_millis(200) {
            last_report = std::time::Instant::now();
            progress(Stage::Download { done, total });
        }
    }
    if out.is_empty() {
        bail!("下载内容为空：{url}");
    }
    progress(Stage::Download { done, total });
    // 粗检：zip 以 "PK" 开头
    if out.len() < 4 || &out[0..2] != b"PK" {
        bail!("下载到的不是 zip（可能被网关/代理替换成了 HTML 错误页）");
    }
    Ok(out)
}

/// 安装根目录。
pub fn install_root(elevated: bool) -> PathBuf {
    if elevated {
        if let Ok(pf) = std::env::var("ProgramFiles") {
            if !pf.trim().is_empty() {
                return PathBuf::from(pf).join("RCGUI").join("rclone");
            }
        }
    }
    per_user_root()
}

fn per_user_root() -> PathBuf {
    match std::env::var("LOCALAPPDATA") {
        Ok(p) if !p.trim().is_empty() => PathBuf::from(p).join("RCGUI").join("rclone"),
        _ => crate::config::data_dir().join("rclone"),
    }
}

/// 目录名只保留安全字符（版本号来自网络，不能直接拼路径）。
fn safe_dir_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "unknown".into()
    } else {
        cleaned
    }
}

fn copy_exe(src: &Path, dst: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("创建安装目录失败：{}", dir.display()))?;
    std::fs::copy(src, dst)
        .with_context(|| format!("复制到 {} 失败（可能被占用或权限不足）", dst.display()))?;
    Ok(())
}

/// 在解压结果里找 `rclone.exe`（深度 ≤ 3，官方包是单层目录）。
pub fn find_rclone_exe(root: &Path) -> Option<PathBuf> {
    fn walk(dir: &Path, depth: usize) -> Option<PathBuf> {
        if depth > 3 {
            return None;
        }
        let mut dirs = Vec::new();
        for e in std::fs::read_dir(dir).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
            } else if p
                .file_name()
                .map(|n| n.to_string_lossy().eq_ignore_ascii_case("rclone.exe"))
                .unwrap_or(false)
            {
                return Some(p);
            }
        }
        for d in dirs {
            if let Some(p) = walk(&d, depth + 1) {
                return Some(p);
            }
        }
        None
    }
    walk(root, 0)
}

// ---------------- 自启 / 停止的一次性闸门 ----------------

/// 启动流程里的「自动启动」只允许跑一次（用户手动重试不受限）。
static AUTO_START_TRIED: AtomicBool = AtomicBool::new(false);

pub fn take_auto_start_slot() -> bool {
    !AUTO_START_TRIED.swap(true, Ordering::SeqCst)
}

// ---------------- 单测 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(exe: &str) -> OneClickCfg {
        OneClickCfg {
            exe: exe.to_string(),
            ..Default::default()
        }
    }

    /// 带用户名的启动参数：`rcd` + 回环地址 + `--rc-user`
    #[test]
    fn launch_args_with_user() {
        let dir = std::env::temp_dir();
        let exe = dir.join("rclone-test-stub.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let mut cfg = cfg_with(&exe.display().to_string());
        cfg.rc_user = "gui".into();
        cfg.rc_pass = String::new();
        cfg.port = 5572;
        let plan = launch_plan(&cfg).unwrap();
        assert_eq!(plan.args[0], "rcd");
        assert_eq!(plan.args[1], "--rc-addr");
        assert_eq!(plan.args[2], "127.0.0.1:5572");
        assert!(plan.args.contains(&"--rc-user".to_string()));
        assert!(!plan.args.contains(&"--rc-pass".to_string()), "空密码不携带");
        assert!(!plan.args.contains(&"--rc-no-auth".to_string()));
        assert!(plan.silent, "默认静默");
        let _ = std::fs::remove_file(&exe);
    }

    /// no_auth：禁用用户/密码，只带 `--rc-no-auth`
    #[test]
    fn launch_args_no_auth_drops_credentials() {
        let exe = std::env::temp_dir().join("rclone-test-stub2.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let mut cfg = cfg_with(&exe.display().to_string());
        cfg.no_auth = true;
        cfg.rc_user = "should-not-appear".into();
        cfg.rc_pass = "secret".into();
        cfg.silent = false;
        let plan = launch_plan(&cfg).unwrap();
        assert!(plan.args.contains(&"--rc-no-auth".to_string()));
        assert!(!plan.args.iter().any(|a| a == "should-not-appear"));
        assert!(!plan.args.iter().any(|a| a == "secret"));
        assert!(!plan.silent);
        let _ = std::fs::remove_file(&exe);
    }

    /// 密码非空则携带；端口为 0 直接报错
    #[test]
    fn launch_args_pass_and_bad_port() {
        let exe = std::env::temp_dir().join("rclone-test-stub3.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let mut cfg = cfg_with(&exe.display().to_string());
        cfg.rc_pass = "p@ss word".into();
        let plan = launch_plan(&cfg).unwrap();
        let i = plan.args.iter().position(|a| a == "--rc-pass").unwrap();
        assert_eq!(plan.args[i + 1], "p@ss word", "含空格也要原样作为一个参数");
        cfg.port = 0;
        assert!(launch_plan(&cfg).is_err());
        let _ = std::fs::remove_file(&exe);
    }

    /// 未设置路径时不允许启动（对应「必须先设置路径」的约束）
    #[test]
    fn launch_requires_exe() {
        let cfg = OneClickCfg::default();
        assert!(!cfg.has_exe());
        assert!(launch_plan(&cfg).is_err());
        assert!(!cfg.auto_start_allowed());
    }

    /// `version.txt` 解析
    #[test]
    fn version_txt_parsing() {
        assert_eq!(parse_version_txt("rclone v1.75.2\n"), Some("v1.75.2".into()));
        assert_eq!(parse_version_txt("v1.70.0"), Some("v1.70.0".into()));
        assert_eq!(parse_version_txt("not a version"), None);
        assert_eq!(parse_version_txt(""), None);
    }

    /// 官方版 / 增强版的 URL 组装
    #[test]
    fn download_urls() {
        assert_eq!(
            official_url("https://downloads.rclone.org/", "v1.75.2", "amd64"),
            "https://downloads.rclone.org/v1.75.2/rclone-v1.75.2-windows-amd64.zip"
        );
        assert_eq!(
            official_url("https://mirrors.nju.edu.cn/rclone", "v1.75.1", "arm64"),
            "https://mirrors.nju.edu.cn/rclone/v1.75.1/rclone-v1.75.1-windows-arm64.zip"
        );
        assert_eq!(
            extra_url("v1.75.2-extra", "amd64"),
            "https://github.com/gulp79/rclone-extra/releases/download/v1.75.2-extra/rclone-windows-amd64.zip"
        );
    }

    /// GitHub release JSON：命中资产名就用它的 browser_download_url
    #[test]
    fn github_release_parsing() {
        let json = serde_json::json!({
            "tag_name": "v1.75.2-extra",
            "assets": [
                {"name": "rclone-linux-amd64.zip", "browser_download_url": "https://x/linux.zip"},
                {"name": "rclone-windows-amd64.zip", "browser_download_url": "https://x/win.zip"}
            ]
        });
        let (tag, url) = parse_github_release(&json, "amd64").unwrap();
        assert_eq!(tag, "v1.75.2-extra");
        assert_eq!(url.as_deref(), Some("https://x/win.zip"));

        // 资产缺失时要给出可读原因（含现有资产名），不能静默拼一个 404 链接
        let err = parse_github_release(&json, "arm64").unwrap_err().to_string();
        assert!(err.contains("rclone-windows-arm64.zip"), "实际: {err}");
    }

    /// 非 PE 文件必须被拒绝（改名的 txt 不能当 rclone）
    #[test]
    fn rejects_non_pe() {
        let p = std::env::temp_dir().join("rcgui-not-rclone.txt");
        std::fs::write(&p, b"hello, i am not a program").unwrap();
        let r = validate(&p.display().to_string());
        assert!(!r.ok);
        assert!(!r.detail.is_empty(), "必须给出可读原因");
        // 「可执行格式」这一行必须是 ✗，且不能因为体积不合格就去执行它
        let pe_row = r
            .rows
            .iter()
            .find(|x| x.label == "可执行格式")
            .expect("应有可执行格式一行");
        assert!(!pe_row.ok, "实际: {}", pe_row.value);
        let ver_row = r
            .rows
            .iter()
            .find(|x| x.label == "版本探测")
            .expect("应有版本探测一行");
        assert!(!ver_row.ok);
        assert!(
            ver_row.value.contains("跳过"),
            "不该去执行明显不是 exe 的文件，实际: {}",
            ver_row.value
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 不存在的路径 / 空路径都要给出明确原因
    #[test]
    fn rejects_missing_and_empty() {
        let r = validate("");
        assert!(!r.ok);
        assert!(r.detail.contains("没有选择") || r.detail.contains("未设置"));
        let r = validate(r"C:\definitely\not\here\rclone.exe");
        assert!(!r.ok);
        assert!(r.detail.contains("不存在"), "实际: {}", r.detail);
    }

    /// 解压目录里找 rclone.exe（单层目录）
    #[test]
    fn finds_exe_in_nested_dir() {
        let root = std::env::temp_dir().join(format!("rcgui-find-exe-{}", std::process::id()));
        let nested = root.join("rclone-v1.75.2-windows-amd64");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("README.txt"), b"doc").unwrap();
        std::fs::write(nested.join("RCLONE.EXE"), b"MZ").unwrap();
        let got = find_rclone_exe(&root).expect("应能找到 rclone.exe");
        assert_eq!(got.file_name().unwrap().to_string_lossy().to_uppercase(), "RCLONE.EXE");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 版本目录名要过滤掉可疑字符（版本号来自网络）
    #[test]
    fn sanitizes_version_dir() {
        assert_eq!(safe_dir_name("v1.75.2"), "v1.75.2");
        assert_eq!(safe_dir_name("../../evil"), ".._.._evil");
        assert_eq!(safe_dir_name(""), "unknown");
    }

    /// 鉴权缺口提示：没勾 no_auth 且用户名密码都空
    #[test]
    fn authless_detection() {
        let mut cfg = OneClickCfg::default();
        assert!(!cfg.authless(), "默认有用户名");
        cfg.rc_user = String::new();
        cfg.rc_pass = String::new();
        assert!(cfg.authless());
        cfg.no_auth = true;
        assert!(!cfg.authless(), "显式 no_auth 不算缺口");
    }
}
