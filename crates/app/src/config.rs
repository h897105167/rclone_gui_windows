//! 应用配置：数据目录解析 + config.json 读写（原子写）
//! 便携模式：exe 旁存在 "portable" 文件时数据目录 = exe目录\data

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// rclone 连接配置。
///
/// **语义（重构指南 §1/§3）**：RCGUI 只连接用户指定的 RC endpoint，
/// 不启动、不终止、不重启 rclone 进程。因此这里没有 exe / 托管模式的概念，
/// 只有「连哪里、用什么凭据、启动时要不要自动连」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RcloneCfg {
    /// 完整 RC endpoint，如 `http://127.0.0.1:5572`
    #[serde(default = "d_rc_url")]
    pub url: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
    /// 启动时自动连接（只是连 API，不启动任何进程）
    #[serde(default = "d_true")]
    pub connect_on_start: bool,
}

fn d_rc_url() -> String {
    "http://127.0.0.1:5572".into()
}

impl Default for RcloneCfg {
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_http_port")]
    pub port: u16,
}
fn d_true() -> bool {
    true
}
fn d_http_port() -> u16 {
    5273
}
impl Default for HttpCfg {
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }
}

/// 自动挂载项（配置跟随远程：远程被删除时级联删除）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoMount {
    /// 远程名（不含冒号，如 "teldrive"）
    pub remote: String,
    /// 远程内子目录（相对路径；空串 = 远程根目录）
    #[serde(default)]
    pub sub_path: String,
    pub point: String,
    #[serde(default)]
    pub ro: bool,
    #[serde(default = "d_cache")]
    pub cache: String,
    /// 是否启用：停用后启动软件时不自动挂载（配置保留）
    #[serde(default = "d_true")]
    pub enabled: bool,
}
fn d_cache() -> String {
    "writes".into()
}

/// 自动服务项（配置跟随远程：远程被删除时级联删除）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoServe {
    pub stype: String,
    /// 远程名（不含冒号）
    pub remote: String,
    /// 远程内子目录（空串 = 远程根目录）
    #[serde(default)]
    pub sub_path: String,
    pub addr: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
    /// 是否启用：停用后启动软件时不自动启动
    #[serde(default = "d_true")]
    pub enabled: bool,
}

/// 服务自定义名称映射（按 协议+远程+子路径+地址 匹配）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServeName {
    pub stype: String,
    pub remote: String,
    #[serde(default)]
    pub sub_path: String,
    pub addr: String,
    pub name: String,
}

fn d_unit() -> i32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub rclone: RcloneCfg,
    /// 0 跟随系统 / 1 浅色 / 2 深色
    #[serde(default)]
    pub theme_mode: u8,
    /// 强调色：0 蓝 / 1 绿 / 2 紫 / 3 橙
    #[serde(default)]
    pub accent_idx: u8,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub silent_start: bool,
    #[serde(default)]
    pub http: HttpCfg,
    /// 插件启用表
    #[serde(default)]
    pub plugin_enabled: std::collections::HashMap<String, bool>,
    /// 自动挂载列表
    ///
    /// **不再写入 config.json**：唯一落盘位置是档案文档
    /// `<数据目录>/profiles/default.json`（见 `profiles.rs`）。这里保留字段是为了
    /// 让既有 UI 代码继续可用，同时仍能读取旧版 config.json 里的同名字段。
    #[serde(default, skip_serializing)]
    pub auto_mounts: Vec<AutoMount>,
    /// 自动服务列表（同上：只从档案文档落盘）
    #[serde(default, skip_serializing)]
    pub auto_serves: Vec<AutoServe>,
    /// 远程路径历史（最近 10 条，最新在前）
    #[serde(default)]
    pub path_history: Vec<String>,
    /// 服务自定义名称映射（同上：只从档案文档落盘）
    #[serde(default, skip_serializing)]
    pub serve_names: Vec<ServeName>,
    /// 代理配置表（同上：只从档案文档落盘）
    #[serde(default, skip_serializing)]
    pub proxies: Vec<rcgui_profile::ProxyProfile>,
    /// remote → 代理绑定（同上）
    #[serde(default, skip_serializing)]
    pub proxy_bindings: Vec<rcgui_profile::RemoteProxyBinding>,
    /// 全局上传限速（数值文本，空 = 不限；rclone 仅支持全局 tx:rx 限速）
    #[serde(default)]
    pub bw_up: String,
    #[serde(default = "d_unit")]
    pub bw_up_unit: i32,
    /// 全局下载限速（数值文本，空 = 不限）
    #[serde(default)]
    pub bw_down: String,
    #[serde(default = "d_unit")]
    pub bw_down_unit: i32,
}

impl Default for AppConfig {
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }
}

impl AppConfig {
    /// 设置某 remote 的代理绑定（`proxy_id` 为空串 = 明确不代理）。
    ///
    /// 换目标时**只清 `applied`，保留脚印**：`applied_value` / `applied_key` /
    /// `original_override` 是回滚的唯一依据，清掉就等于“从未管理过”，
    /// 切回「不代理」时 apply 会判定无需动作，rclone 里的旧代理永远删不掉。
    /// 与 `ProfileDoc::bind_proxy` 的语义保持一致。
    pub fn bind_proxy_slot(&mut self, remote: &str, proxy_id: &str) {
        let key = remote.trim_end_matches(':');
        match self
            .proxy_bindings
            .iter_mut()
            .find(|b| b.remote.eq_ignore_ascii_case(key))
        {
            Some(b) => {
                if b.proxy_id != proxy_id {
                    b.proxy_id = proxy_id.to_string();
                    b.applied = false;
                }
            }
            None => self.proxy_bindings.push(rcgui_profile::RemoteProxyBinding {
                remote: key.to_string(),
                proxy_id: proxy_id.to_string(),
                ..Default::default()
            }),
        }
    }
}

/// 应用数据根目录。
///
/// - 便携模式（exe 旁边有 `portable` 文件）：`exe目录\data`
/// - 否则：`%APPDATA%\RCGUI`，并在首次启动时从旧目录 `%APPDATA%\rclone-gui` **复制**迁移。
///
/// 迁移原则（指南 §4）：复制而不是移动，旧目录**不删除、不修改**，随时可回滚；
/// 迁移失败则继续使用旧目录，绝不因为换目录导致用户数据「消失」。
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        if dir.join("portable").exists() {
            return dir.join("data");
        }
    }
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    // 展开可能的 8.3 短名（ADMINI~1 → Administrator），保证目录名与提示都正确
    let appdata = long_path(&PathBuf::from(appdata));
    let dir = resolve_data_dir(appdata.join("RCGUI"), appdata.join("rclone-gui"));
    // 确保本机用户对数据目录有写权限。
    //
    // 背景：如果程序曾以管理员身份运行过，它创建的目录 owner 会是
    // `BUILTIN\Administrators`，ACL 里往往只有 SYSTEM / Administrators / Local Account，
    // 没有普通用户或 Users 组的显式 ACE。用户之后以普通权限启动时，
    // 令牌里的 Administrators 是「仅用于拒绝」，于是任何写入都报
    // `拒绝访问 (os error 5)` —— 现象像配置错误，实则是目录权限。
    //
    // 只在「确实缺 Users 权限」时执行，避免每次取数据目录都去 spawn icacls。
    ensure_user_can_write_once(&dir);
    dir
}

/// 每个进程只尝试修复一次，且只在**确实缺少 Users 权限**时才真正执行。
///
/// 用 `OnceLock` 而不是 `Once`：需要记住结果供后续诊断。
static PERM_FIXED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn ensure_user_can_write_once(dir: &std::path::Path) {
    PERM_FIXED.get_or_init(|| {
        // 先做「真实可写性探测」，而不是只看 ACL 里有没有 Users。
        //
        // 只看 ACL 会误判：ACL 里可能有继承来的 `Users:(I)(F)` 条目，
        // 但文件 owner 是 `BUILTIN\Administrators`（管理员权限写过），
        // 普通权限进程在部分配置下仍然无法覆盖 —— 表现为「明明有权限却拒绝访问」。
        // 因此这里直接试着建一个临时文件，失败才去修。
        let writable = probe_writable(dir);
        debug_log(&format!(
            "[perm] dir={} writable={}",
            dir.display(),
            writable
        ));
        if writable {
            return true;
        }
        let ok = ensure_user_can_write(dir, true);
        let after = probe_writable(dir);
        debug_log(&format!("[perm] repair attempted -> {ok}, writable_after={after}"));
        ok && after
    });
}

/// 在当前目录真实建一个临时文件来判断可写性。
///
/// 这是最可靠的判断方式：ACL 看起来正确不代表真的能写（owner、完整性级别、
/// 继承标志都可能让写入失败），只有实际写一次才不会骗人。
fn probe_writable(dir: &std::path::Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".rcgui-wprobe-{}", std::process::id()));
    match std::fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 极简诊断日志：写到 `%APPDATA%\RCGUI\perm-debug.log`。
///
/// 仅用于排查「拒绝访问」这类难以复现的问题；写入失败完全忽略。
/// 注意**不能**调用 `data_dir()`（会递归进 `ensure_user_can_write_once`），
/// 所以这里自己拼路径。
fn debug_log(msg: &str) {
    if std::env::var("RCGUI_DEBUG_PERM").is_err() {
        return;
    }
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    let path = std::path::PathBuf::from(base).join("RCGUI").join("perm-debug.log");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{} {msg}", rcgui_profile::now_iso());
    }
}


/// 确保本目录对**普通用户**可写（补一条 `BUILTIN\Users` 的显式 ACE 并递归）。
///
/// 为什么授予 `Users` 而不是「当前用户名」：
/// - 目录 owner 可能是 `BUILTIN\Administrators`（管理员权限运行时创建）时，
///   非提升进程不能把 ACL 改成给「自己」授权，实测返回 ERROR_ACCESS_DENIED(5)；
/// - 给内置 `BUILTIN\Users` 组加 ACE 则是允许的，且覆盖本机所有交互用户。
///
/// ⚠️ 关键坑：**`icacls` 的退出码不可靠**。带 `/C` 时它「继续处理剩余文件」，
/// 即使某个路径报「拒绝访问」也仍然返回 0。因此必须检查输出里是否出现失败字样，
/// 不能只看 `status.success()`。这一条曾经让自愈逻辑「看起来成功、实际没生效」。
fn ensure_user_can_write(dir: &std::path::Path, quiet: bool) -> bool {
    if !dir.exists() {
        return false;
    }
    // 用绝对路径：GUI 进程的 PATH 不保证包含 System32
    let icacls = std::env::var("SystemRoot")
        .map(|r| format!(r"{r}\System32\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string());
    let out = std::process::Command::new(&icacls)
        .arg(dir)
        .arg("/grant")
        .arg("*S-1-5-32-545:(OI)(CI)F") // BUILTIN\Users，用 SID 避免本地化名称差异
        .arg("/T")
        .arg("/C")
        .arg("/Q")
        .creation_flags_no_window()
        .output();
    let _ = quiet;
    match out {
        Ok(o) => {
            if !o.status.success() {
                return false;
            }
            // exit code 为 0 不代表全部成功：扫输出里的失败信号
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            let combined = format!("{stdout}{stderr}");
            !(combined.contains("拒绝访问")
                || combined.contains("Access is denied")
                || combined.contains("失败")
                || combined.contains("Failed processing"))
        }
        Err(_) => false,
    }
}

/// 把 8.3 短名（如 `ADMINI~1`）解析成真实长路径。
///
/// `APPDATA` / `TEMP` 这类环境变量在某些会话里是短名形式，拼出来的路径
/// 人类看不懂，传给资源管理器也可能打不开。`canonicalize` 会展开短名。
fn long_path(p: &std::path::Path) -> PathBuf {
    match std::fs::canonicalize(p) {
        Ok(c) => {
            let s = c.to_string_lossy().to_string();
            PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
        }
        Err(_) => p.to_path_buf(),
    }
}/// 选择数据目录，必要时执行一次性迁移。
fn resolve_data_dir(new_dir: PathBuf, legacy_dir: PathBuf) -> PathBuf {
    // 新目录已存在（已迁移过/全新安装）→ 直接用
    if new_dir.exists() || !legacy_dir.exists() {
        return new_dir;
    }
    match copy_tree(&legacy_dir, &new_dir) {
        Ok((files, bytes, skipped)) => {
            let marker = serde_json::json!({
                "schema_version": 1,
                "from": legacy_dir.display().to_string(),
                "to": new_dir.display().to_string(),
                "migrated_at": rcgui_profile::now_iso(),
                "files": files,
                "bytes": bytes,
                "skipped": skipped,
                // 回滚 = 删除新目录即可，旧目录从未被修改
                "rollback": "删除该 RCGUI 目录后重启，程序会回到旧目录 %APPDATA%\\rclone-gui"
            });
            let _ = write_atomic(&new_dir.join("migration.json"), marker.to_string().as_bytes());
            let mut msg = format!(
                "数据已从旧目录迁移到 {}（{} 个文件 / {:.1} MB；旧目录保留，可回滚）",
                new_dir.display(),
                files,
                bytes as f64 / 1024.0 / 1024.0
            );
            if !skipped.is_empty() {
                msg.push_str(&format!("；{} 个条目被跳过（详见 migration.json）", skipped.len()));
            }
            crate::profiles::push_notice(msg);
            new_dir
        }
        Err(e) => {
            // 只清理本次自己创建的不完整副本（进入本函数时 new_dir 尚不存在）
            let _ = std::fs::remove_dir_all(&new_dir);
            crate::profiles::push_notice(format!(
                "数据目录迁移失败（{e}），本次继续使用旧目录 {}",
                legacy_dir.display()
            ));
            legacy_dir
        }
    }
}

/// 递归复制目录树（跳过符号链接等特殊条目）。
///
/// **尽力而为**：单个文件失败（典型情况是 `rcd.log` 仍被运行中的 rclone 占用）
/// 只记为 skipped，不中断整个迁移——否则用户只要开着旧版 rclone，数据目录就永远迁不过去。
/// 返回 (文件数, 字节数, 跳过的条目)。
fn copy_tree(src: &Path, dst: &Path) -> Result<(u64, u64, Vec<String>)> {
    fn walk(
        src: &Path,
        dst: &Path,
        files: &mut u64,
        bytes: &mut u64,
        skipped: &mut Vec<String>,
    ) {
        if let Err(e) = std::fs::create_dir_all(dst) {
            skipped.push(format!("{}（{e}）", dst.display()));
            return;
        }
        let rd = match std::fs::read_dir(src) {
            Ok(rd) => rd,
            Err(e) => {
                skipped.push(format!("{}（{e}）", src.display()));
                return;
            }
        };
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    skipped.push(format!("{} 内部条目（{e}）", src.display()));
                    continue;
                }
            };
            let path = entry.path();
            let to = dst.join(entry.file_name());
            // 用 symlink_metadata 判定类型：不跟随符号链接，避免复制目标逃出数据目录
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    skipped.push(format!("{}（{e}）", path.display()));
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                skipped.push(format!("{}（符号链接）", path.display()));
            } else if meta.is_dir() {
                walk(&path, &to, files, bytes, skipped);
            } else if meta.is_file() {
                match std::fs::copy(&path, &to) {
                    Ok(n) => {
                        *files += 1;
                        *bytes += n;
                    }
                    Err(e) => skipped.push(format!("{}（{e}）", path.display())),
                }
            } else {
                skipped.push(format!("{}（特殊文件）", path.display()));
            }
        }
    }
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut skipped: Vec<String> = Vec::new();
    // 只有「连根目录都建不出来」才算致命错误
    std::fs::create_dir_all(dst)
        .map_err(|e| anyhow::anyhow!("创建 {} 失败: {e}", dst.display()))?;
    walk(src, dst, &mut files, &mut bytes, &mut skipped);
    Ok((files, bytes, skipped))
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// 写入失败时的自愈：给目录补上「当前用户」的完全控制权限。
///
/// **为什么需要这个**：如果程序曾经以「管理员身份」运行过，它创建的
/// `%APPDATA%\RCGUI` 及其子目录会带上这样一份 ACL：
///
/// ```text
/// O:BAG:S-1-5-21-...-513D:(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)(A;OICIID;FA;;;LA)
/// ```
///
/// 只有 `SY`（SYSTEM）、`BA`（Administrators）、`LA`（Local Account）三条，
/// **没有 `Users` / 当前用户的显式 ACE**。而普通（非提升）进程的令牌里，
/// `BUILTIN\Administrators` 是「仅用于拒绝」(deny only)，`LA` 也不包含
/// 本机交互用户 —— 结果就是**普通权限下完全无法写入**，报
/// `拒绝访问 (os error 5)`。
///
/// 用户视角的现象极难诊断：配置明明没问题、文件也在，就是存不进去；
/// 而且往往是「以管理员跑过一次之后」才开始坏。
///
/// 这里用 `icacls` 给当前用户补 ACE（继承到子项），成功后原操作重试一次。
/// 用 `icacls` 而不是自己拼 SDDL：ACL 编辑涉及精确的 ACE 顺序与传播标志，
/// 交给系统工具更可靠，也便于用户用同样命令手动修复。
pub fn repair_data_dir_permissions() -> Result<(), String> {
    let dir = data_dir();
    if !dir.exists() {
        return Err(format!("数据目录不存在：{}", dir.display()));
    }
    if ensure_user_can_write(&dir, false) {
        Ok(())
    } else {
        let account = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
            (Ok(d), Ok(u)) if !d.is_empty() && !u.is_empty() => format!("{d}\\{u}"),
            (_, Ok(u)) => u,
            _ => "<你的用户名>".to_string(),
        };
        Err(format!(
            "自动修复数据目录权限失败。可手动执行：icacls \"{}\" /grant \"{account}:(OI)(CI)F\" /T",
            dir.display()
        ))
    }
}

/// 给 `std::process::Command` 加「不弹黑框」的 Windows 扩展。
trait NoWindowExt {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

impl NoWindowExt for std::process::Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW = 0x08000000，避免弹一下控制台
            self.creation_flags(0x08000000);
        }
        self
    }
}

/// 写入失败时统一的自愈包装：失败 → 尝试修权限 → 重试一次。
///
/// 只在「权限类错误」时才修，避免任何写失败都去动 ACL（那会让真正的
/// 磁盘满、文件被占用等问题更难诊断）。
pub fn write_atomic(file: &PathBuf, data: &[u8]) -> Result<()> {
    match write_atomic_once(file, data) {
        Ok(()) => Ok(()),
        Err(e) if is_permission_error(&e) => {
            repair_data_dir_permissions().map_err(|re| {
                anyhow::anyhow!(
                    "{e}；自动修复权限也失败了：{re}",
                    e = e
                )
            })?;
            // 修完重试一次；这次仍失败就把原始错误抛出去
            write_atomic_once(file, data)
        }
        Err(e) => Err(e),
    }
}

fn write_atomic_once(file: &PathBuf, data: &[u8]) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

/// 判断错误是否是「权限/拒绝访问」类。
fn is_permission_error(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::PermissionDenied {
                return true;
            }
            // Windows: ERROR_ACCESS_DENIED = 5
            if io.raw_os_error() == Some(5) {
                return true;
            }
        }
    }
    false
}

pub fn load() -> AppConfig {
    let mut cfg = load_raw();
    // 档案文档是「挂载 / 服务 / 预设名」的权威来源。
    // 档案不存在时，profiles 模块会把 config.json 里的旧字段迁移过去并落盘。
    if let Some(doc) = crate::profiles::load_doc(&cfg) {
        crate::profiles::apply_doc(&doc, &mut cfg);
    }
    cfg
}

/// 只读 config.json（应用偏好）。旧版本里可能仍带有挂载/服务字段，由调用方用档案覆盖。
fn load_raw() -> AppConfig {
    let p = config_path();
    if let Ok(raw) = std::fs::read(&p) {
        // 容忍 UTF-8 BOM（外部工具编辑配置文件时可能引入）
        let raw: &[u8] = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) { &raw[3..] } else { &raw };
        if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(raw) {
            migrate(&mut v);
            if let Ok(cfg) = serde_json::from_value::<AppConfig>(v) {
                return cfg;
            }
        }
    }
    AppConfig::default()
}

/// 旧格式迁移：auto_mounts/auto_serves/serve_names 中的
/// `fs: "name:sub/path"` 拆为 `remote: "name"` + `sub_path: "sub/path"`
fn migrate(v: &mut serde_json::Value) {
    migrate_rclone(v);
    /// "name:sub" → ("name", "sub")；"name:" → ("name", "")
    fn split_fs(fs: &str) -> (String, String) {
        match fs.find(':') {
            Some(i) => (
                fs[..i].trim().to_string(),
                fs[i + 1..].trim_matches('/').trim().to_string(),
            ),
            None => (fs.trim_end_matches(':').trim().to_string(), String::new()),
        }
    }

    let keys = ["auto_mounts", "auto_serves", "serve_names"];
    for key in keys {
        let Some(arr) = v.get_mut(key).and_then(|x| x.as_array_mut()) else {
            continue;
        };
        for it in arr.iter_mut() {
            // 仅迁移仍是旧格式（有 fs 无 remote）的条目
            if it.get("remote").is_none() {
                if let Some(fs) = it.get("fs").and_then(|x| x.as_str()) {
                    let (remote, sub) = split_fs(fs);
                    if let Some(obj) = it.as_object_mut() {
                        obj.insert("remote".into(), serde_json::json!(remote));
                        obj.insert("sub_path".into(), serde_json::json!(sub));
                        obj.remove("fs");
                    }
                }
            }
        }
    }
}

/// 旧 `rclone.mode/port/exe` → 新连接模型（`url` + `connect_on_start`）。
///
/// 迁移规则（按重构任务要求）：
/// - 旧 `mode`（0 托管 / 1 连接已有实例）**一律降级为「连接」语义**：不再自动启动进程；
/// - 旧托管模式本来就只认 `127.0.0.1:<port>`，因此用 `port` 拼出 endpoint；
/// - 旧连接模式保留用户填的 `url`；
/// - `connect_on_start` 默认 true：启动时尝试连接这个 endpoint，连不上就进离线模式。
fn migrate_rclone(v: &mut serde_json::Value) {
    let Some(rc) = v.get_mut("rclone").and_then(|x| x.as_object_mut()) else {
        return;
    };
    let had_mode = rc.contains_key("mode");
    let had_port = rc.get("port").and_then(|p| p.as_u64());
    let had_exe = rc.contains_key("exe");
    // 已经是新格式（没有旧字段）→ 不做任何改动，避免把默认值写坏
    if !had_mode && had_port.is_none() && !had_exe {
        return;
    }
    let mode = rc.get("mode").and_then(|m| m.as_u64()).unwrap_or(1);
    let url = rc
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let fallback = d_rc_url();
    let port_url: Option<String> = had_port
        .filter(|p| *p > 0)
        .map(|p| format!("http://127.0.0.1:{p}"));
    let mut new_url = url.clone();
    if mode == 0 {
        // 旧托管模式：原先连的就是 127.0.0.1:<port>，优先用端口拼
        if let Some(p) = port_url.clone() {
            new_url = p;
        } else if new_url.is_empty() {
            new_url = fallback.clone();
        }
    } else if new_url.is_empty() {
        new_url = port_url.unwrap_or(fallback);
    }
    if let Some(obj) = v.get_mut("rclone").and_then(|x| x.as_object_mut()) {
        obj.insert("url".into(), serde_json::json!(new_url));
        obj.entry("connect_on_start").or_insert(serde_json::json!(true));
        // 旧字段清除：不再有「托管模式」和「自带 rclone.exe」这两个概念
        obj.remove("mode");
        obj.remove("port");
        obj.remove("exe");
        obj.remove("exe_custom");
    }
}

pub fn save(cfg: &AppConfig) -> Result<()> {
    // 顺序很重要：档案先写，失败就整体放弃。
    // 因为 config.json 已不再包含挂载/服务字段（skip_serializing），若先写 config.json
    // 而档案写失败，这些预设就只剩内存里一份，重启即丢。
    crate::profiles::save_from_cfg(cfg)?;
    // 第二步：应用偏好（已不含挂载/服务列表）
    let data = serde_json::to_vec_pretty(cfg)?;
    write_atomic(&config_path(), &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgui_profile::ProxyProfile;

    /// `bind_proxy_slot` 换目标时只清 `applied`，保留脚印（回滚依据）。
    ///
    /// 2026-10-18 修掉的缺陷：清掉 `applied_value` 后，切「不代理」时
    /// `plan_apply` 会判定「从未管理过」，rclone 里的旧代理永远删不掉。
    #[test]
    fn rebinding_keeps_rollback_footprint() {
        let mut cfg = AppConfig::default();
        cfg.proxies.push(ProxyProfile {
            id: "p1".into(),
            name: "n".into(),
            ..Default::default()
        });
        cfg.bind_proxy_slot("r", "p1");
        {
            let b = &mut cfg.proxy_bindings[0];
            b.applied = true;
            b.applied_key = "http_proxy".into();
            b.applied_value = "http://ours:1".into();
            b.original_override = None;
        }
        cfg.bind_proxy_slot("r", "");
        let b = &cfg.proxy_bindings[0];
        assert!(b.is_direct());
        assert!(!b.applied, "换目标后要标成待应用");
        assert_eq!(b.applied_value, "http://ours:1", "回滚依据不能被清掉");
        assert_eq!(b.applied_key, "http_proxy");
        assert!(b.has_footprint());
    }
}
