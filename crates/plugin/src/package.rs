//! 插件包（plugin package）：目录形态的插件分发格式。
//!
//! 一个插件包是一个目录，至少包含：
//! ```text
//! myplugin/
//!   plugin.json        # 元数据（id / name / version / entry_file ...）
//!   manifest.sha256    # 完整性校验表："<sha256>  <相对路径>" 每行一条
//!   <entry_file>       # 入口文件（.msi / .exe / .bat / .ps1 ...）
//! ```
//!
//! 与 WASM 插件（`imagehost`）的区别：WASM 插件是单个 `.wasm`，由本进程内的
//! wasmtime 执行；插件包是"外部程序"，启用时由宿主**运行其 entry_file**
//! （例如安装 WinFsp 的 msi），因此它的启用状态是"是否已执行安装"。

use anyhow::{anyhow, bail, Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::zip;

/// plugin.json 的内容。
///
/// 字段设计说明：
/// - `id` 用反向域名，保证全局唯一（升级/替换按 id 判定）
/// - `version` 用语义化版本，升级判定依赖它（semver 比较，而非字符串比较）
/// - `min_gui_version` 用于拒绝"给新版 GUI 做的插件装到旧版 GUI 上"
/// - `entry_file` 是启用时执行的文件，必须出现在 manifest.sha256 中
/// - `entry_args` / `silent_args` 用于免交互安装（如 msiexec /qn）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PluginJson {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    /// 最低支持的宿主版本；为空表示不限制
    #[serde(default)]
    pub min_gui_version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// 入口文件（相对插件包根目录）
    #[serde(default)]
    pub entry_file: String,
    /// 运行 entry_file 时的附加参数
    #[serde(default)]
    pub entry_args: Vec<String>,
    /// 平台限制，如 "windows"；为空表示不限
    #[serde(default)]
    pub platform: String,
    /// 插件主页（可选）
    #[serde(default)]
    pub homepage: String,
    /// 许可证标识（可选）
    #[serde(default)]
    pub license: String,
    /// 是否需要管理员权限才能执行 entry_file
    #[serde(default)]
    pub requires_admin: bool,
    /// 卸载时执行的命令（可选，相对插件包根目录）
    #[serde(default)]
    pub uninstall_file: String,
    #[serde(default)]
    pub uninstall_args: Vec<String>,
}

impl PluginJson {
    /// 校验必需字段与取值合法性。
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("plugin.json 缺少 id");
        }
        // id 必须像反向域名，避免与 WASM 插件或其它包混淆
        if !self.id.contains('.') || self.id.contains(char::is_whitespace) {
            bail!("plugin.json 的 id 格式不合法（应类似 com.example.plugin）：{}", self.id);
        }
        if self.name.trim().is_empty() {
            bail!("plugin.json 缺少 name");
        }
        if self.version.trim().is_empty() {
            bail!("plugin.json 缺少 version");
        }
        Version::parse(self.version.trim())
            .with_context(|| format!("plugin.json 的 version 不是合法语义化版本：{}", self.version))?;
        if !self.min_gui_version.trim().is_empty() {
            Version::parse(self.min_gui_version.trim()).with_context(|| {
                format!("plugin.json 的 min_gui_version 不是合法语义化版本：{}", self.min_gui_version)
            })?;
        }
        if self.entry_file.trim().is_empty() {
            bail!("plugin.json 缺少 entry_file");
        }
        // 与 [`crate::config::PluginConfig::validate`] 一致：WASI 插件不挑平台，
        // `any`（或不填）与 `windows` 都放行，其余拒绝。
        let platform = self.platform.trim();
        if !platform.is_empty() && platform != "windows" && platform != "any" {
            bail!("插件声明的平台不受支持：{platform}（仅支持 windows / any）");
        }
        Ok(())
    }

    /// 宿主版本是否满足 min_gui_version
    pub fn compatible_with(&self, gui_version: &str) -> Result<()> {
        if self.min_gui_version.trim().is_empty() {
            return Ok(());
        }
        let need = Version::parse(self.min_gui_version.trim())?;
        let have = Version::parse(gui_version.trim())
            .with_context(|| format!("宿主版本号无法解析：{gui_version}"))?;
        if have < need {
            bail!(
                "插件需要宿主版本 >= {}，当前为 {}",
                self.min_gui_version,
                gui_version
            );
        }
        Ok(())
    }
}

/// manifest.sha256 中的一条记录
#[derive(Debug, Clone, PartialEq)]
pub struct HashEntry {
    pub sha256: String,
    pub path: String,
}

/// 解析 manifest.sha256。
///
/// 格式与 `sha256sum` 一致：每行 `<64位十六进制>  <相对路径>`，
/// 兼容 `*` 二进制标记与 `#` 注释、空行。
pub fn parse_manifest(text: &str) -> Result<Vec<HashEntry>> {
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // 分隔符是空白；路径可能含空格，因此按"第一个空白段"切分
        let mut it = line.splitn(2, char::is_whitespace);
        let hash = it.next().unwrap_or("").trim();
        let rest = it.next().unwrap_or("").trim();
        // 去掉 sha256sum 的二进制标记 "*"
        let path = rest.strip_prefix('*').unwrap_or(rest).trim();
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!(
                "manifest.sha256 第 {} 行的哈希不合法（应为 64 位十六进制）",
                lineno + 1
            );
        }
        if path.is_empty() {
            bail!("manifest.sha256 第 {} 行缺少文件路径", lineno + 1);
        }
        out.push(HashEntry {
            sha256: hash.to_ascii_lowercase(),
            path: path.replace('\\', "/"),
        });
    }
    if out.is_empty() {
        bail!("manifest.sha256 为空，无法校验插件完整性");
    }
    Ok(out)
}

/// 校验结果
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub checked: usize,
    /// 校验通过的文件（相对路径）
    pub verified: Vec<String>,
}

/// 校验目录下的插件包完整性：manifest.sha256 里列出的每个文件都必须存在且哈希一致。
///
/// 注意：这里只校验 manifest **列出**的文件，不要求目录里没有其它文件
/// （插件运行时可能生成缓存）。但 entry_file 必须在清单中，否则无法保证
/// 被执行的文件未被篡改。
pub fn verify_dir(dir: &Path, pj: &PluginJson) -> Result<VerifyReport> {
    let mf = dir.join("manifest.sha256");
    let text = std::fs::read_to_string(&mf)
        .with_context(|| format!("读取 manifest.sha256 失败: {}", mf.display()))?;
    let entries = parse_manifest(&text)?;

    let entry_rel = pj.entry_file.replace('\\', "/");
    if !entries.iter().any(|e| e.path == entry_rel) {
        bail!("manifest.sha256 未包含入口文件 {entry_rel}，拒绝加载");
    }

    let mut verified = Vec::new();
    for e in &entries {
        let p = zip::safe_join(dir, &e.path)
            .with_context(|| format!("manifest.sha256 中的路径不合法: {}", e.path))?;
        if !p.is_file() {
            bail!("插件缺少清单中声明的文件: {}", e.path);
        }
        let got = zip::sha256_file(&p)?;
        if got != e.sha256 {
            bail!(
                "插件文件校验失败: {}\n  期望 {}\n  实际 {}",
                e.path,
                e.sha256,
                got
            );
        }
        verified.push(e.path.clone());
    }
    Ok(VerifyReport {
        checked: entries.len(),
        verified,
    })
}

/// 从目录读取并校验一个插件包
pub fn load_from_dir(dir: &Path, gui_version: &str) -> Result<(PluginJson, VerifyReport)> {
    let pj_path = dir.join("plugin.json");
    let text = std::fs::read_to_string(&pj_path)
        .with_context(|| format!("读取 plugin.json 失败: {}", pj_path.display()))?;
    let pj: PluginJson = serde_json::from_str(&text)
        .with_context(|| format!("解析 plugin.json 失败: {}", pj_path.display()))?;
    pj.validate()?;
    pj.compatible_with(gui_version)?;
    let report = verify_dir(dir, &pj)?;
    Ok((pj, report))
}

/// 版本比较：返回 true 表示 `new` 比 `old` 新。
/// 版本号无法解析时按"不更新"处理（保守，避免用坏版本覆盖好版本）。
pub fn is_newer(new: &str, old: &str) -> bool {
    match (Version::parse(new.trim()), Version::parse(old.trim())) {
        (Ok(n), Ok(o)) => n > o,
        _ => false,
    }
}

/// 已安装插件的扫描结果
#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub meta: PluginJson,
    pub dir: PathBuf,
    pub verified: bool,
}

/// 扫描插件根目录下的所有插件包（每个子目录一个插件）。
///
/// 单个插件损坏不应影响其它插件：这里收集错误而不是直接返回失败。
pub fn scan_dir(root: &Path, gui_version: &str) -> (Vec<InstalledPlugin>, Vec<(PathBuf, String)>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return (ok, bad);
    };
    for entry in rd.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        if !dir.join("plugin.json").is_file() {
            continue; // 不是插件包目录（例如 WASM 插件的数据目录）
        }
        match load_from_dir(&dir, gui_version) {
            Ok((meta, _)) => ok.push(InstalledPlugin {
                meta,
                dir,
                verified: true,
            }),
            Err(e) => bad.push((dir, format!("{e:#}"))),
        }
    }
    // 稳定顺序，便于 UI 显示与测试
    ok.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));
    bad.sort_by(|a, b| a.0.cmp(&b.0));
    (ok, bad)
}

/// 从 zip 字节流读取插件元数据（不落盘）。
pub fn peek_zip_meta(data: &[u8]) -> Result<PluginJson> {
    let (_name, content) = zip::find_plugin_json(data)?
        .ok_or_else(|| anyhow!("压缩包内未找到 plugin.json，这不是一个插件包"))?;
    let pj: PluginJson = serde_json::from_slice(&content).context("解析 plugin.json 失败")?;
    pj.validate()?;
    Ok(pj)
}

// ---------------- 安装 ----------------

/// 安装来源的大小限制。
///
/// 太小几乎不可能是有效插件包；太大则解压既慢又占空间，
/// 且正常的"安装器型插件"不会到这个量级。
pub const MIN_PACKAGE_BYTES: u64 = 256;
pub const MAX_PACKAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB
/// 解压后的总体积上限（防 zip 炸弹）
pub const MAX_EXTRACTED_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GB

/// 安装动作的结果，用于给 UI 反馈
#[derive(Debug, Clone, PartialEq)]
pub enum InstallOutcome {
    /// 新插件已添加
    Added { id: String, version: String },
    /// 已存在且版本更高 → 已替换
    Upgraded {
        id: String,
        from: String,
        to: String,
    },
    /// 已存在，版本相同或更低 → 未替换
    Kept {
        id: String,
        installed: String,
        offered: String,
    },
}

impl InstallOutcome {
    pub fn message(&self) -> String {
        match self {
            InstallOutcome::Added { id, version } => format!("已添加插件 {id} v{version}"),
            InstallOutcome::Upgraded { id, from, to } => {
                format!("已更新插件 {id}：v{from} → v{to}")
            }
            InstallOutcome::Kept {
                id,
                installed,
                offered,
            } => format!("插件 {id} 已安装 v{installed}，来源版本 v{offered} 不更高，未替换"),
        }
    }
}

/// 检查来源文件大小是否合理。
pub fn check_source_size(bytes: u64) -> Result<()> {
    if bytes < MIN_PACKAGE_BYTES {
        bail!(
            "文件太小（{bytes} 字节），不像是一个插件包（至少 {MIN_PACKAGE_BYTES} 字节）"
        );
    }
    if bytes > MAX_PACKAGE_BYTES {
        bail!(
            "文件太大（{} MB），超过上限 {} MB，拒绝处理",
            bytes / 1024 / 1024,
            MAX_PACKAGE_BYTES / 1024 / 1024
        );
    }
    Ok(())
}

/// 把已校验的插件目录安装到 `plugins_root/<id>`。
///
/// 规则：
/// - 新 id → 直接安装
/// - 已存在且新版本更高 → 替换（旧目录先移到备份名，成功后再删）
/// - 已存在且版本相同或更低 → 不替换
///
/// `src_dir` 必须已经通过 `load_from_dir` 校验。
pub fn install_verified_dir(
    src_dir: &Path,
    plugins_root: &Path,
    gui_version: &str,
) -> Result<InstallOutcome> {
    let (meta, _) = load_from_dir(src_dir, gui_version)?;
    std::fs::create_dir_all(plugins_root)?;
    let dest = plugins_root.join(sanitize_dir_name(&meta.id));

    let existing = if dest.join("plugin.json").is_file() {
        load_from_dir(&dest, gui_version).ok().map(|(m, _)| m)
    } else {
        None
    };

    match existing {
        Some(old) if !is_newer(&meta.version, &old.version) => Ok(InstallOutcome::Kept {
            id: meta.id,
            installed: old.version,
            offered: meta.version,
        }),
        Some(old) => {
            // 替换：先把旧目录挪到旁边，避免复制中途失败导致新旧都不完整
            let backup = plugins_root.join(format!("{}.old-{}", sanitize_dir_name(&meta.id), std::process::id()));
            let _ = std::fs::remove_dir_all(&backup);
            std::fs::rename(&dest, &backup)
                .with_context(|| format!("备份旧版本失败: {}", dest.display()))?;
            match copy_dir(src_dir, &dest) {
                Ok(()) => {
                    let _ = std::fs::remove_dir_all(&backup);
                    Ok(InstallOutcome::Upgraded {
                        id: meta.id,
                        from: old.version,
                        to: meta.version,
                    })
                }
                Err(e) => {
                    // 回滚，保证用户原有的插件不被破坏
                    let _ = std::fs::remove_dir_all(&dest);
                    let _ = std::fs::rename(&backup, &dest);
                    Err(e.context("替换插件失败，已回滚到旧版本"))
                }
            }
        }
        None => {
            copy_dir(src_dir, &dest)?;
            Ok(InstallOutcome::Added {
                id: meta.id,
                version: meta.version,
            })
        }
    }
}

/// 从 zip 字节流安装插件（先解压到临时目录并校验，再落盘）。
pub fn install_from_zip(
    data: &[u8],
    plugins_root: &Path,
    tmp_root: &Path,
    gui_version: &str,
) -> Result<InstallOutcome> {
    check_source_size(data.len() as u64)?;
    // 先看元数据（不解压全部），尽早拒绝非插件包
    let meta = peek_zip_meta(data)?;

    let tmp = tmp_root.join(format!("unpack-{}-{}", std::process::id(), sanitize_dir_name(&meta.id)));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let result = (|| -> Result<InstallOutcome> {
        zip::extract_all(data, &tmp, MAX_EXTRACTED_BYTES)?;
        // 若解压出来的是"单层子目录"，把该层提上来（很多打包工具会自带一层目录）
        let root = unwrap_single_dir(&tmp)?;
        install_verified_dir(&root, plugins_root, gui_version)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// 若目录下只有一个子目录且没有文件，则返回该子目录（解开打包工具自带的顶层目录）。
fn unwrap_single_dir(dir: &Path) -> Result<PathBuf> {
    let mut dirs = Vec::new();
    let mut files = 0usize;
    for e in std::fs::read_dir(dir)?.flatten() {
        if e.path().is_dir() {
            dirs.push(e.path());
        } else {
            files += 1;
        }
    }
    if files == 0 && dirs.len() == 1 {
        return Ok(dirs.remove(0));
    }
    Ok(dir.to_path_buf())
}

/// 把目录内容复制到目标（递归）
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("复制失败: {}", from.display()))?;
        }
    }
    Ok(())
}

/// id 用作目录名：只保留安全字符，防止路径穿越
pub fn sanitize_dir_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 执行插件的 entry_file（启用插件 = 运行入口文件）。
///
/// 支持的入口类型：
/// - `.msi`  → `msiexec /i "<file>" /qn`（静默安装）
/// - `.exe`  → 直接运行，附加 `entry_args`
/// - `.bat` / `.cmd` → `cmd /c`
/// - `.ps1`  → `powershell -ExecutionPolicy Bypass -File`
///
/// 返回子进程退出码（0 表示成功）。
pub fn run_entry_file(pkg_dir: &Path, meta: &PluginJson) -> Result<i32> {
    let entry = zip::safe_join(pkg_dir, &meta.entry_file)?;
    if !entry.is_file() {
        bail!("入口文件不存在: {}", entry.display());
    }
    let ext = entry
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut cmd = match ext.as_str() {
        "msi" => {
            let mut c = std::process::Command::new("msiexec");
            c.arg("/i").arg(&entry);
            // 静默安装：不弹 UI，失败不重启
            c.arg("/qn").arg("/norestart");
            c
        }
        "exe" => std::process::Command::new(&entry),
        "bat" | "cmd" => {
            let mut c = std::process::Command::new("cmd");
            c.arg("/c").arg(&entry);
            c
        }
        "ps1" => {
            let mut c = std::process::Command::new("powershell");
            c.arg("-NoProfile")
                .arg("-ExecutionPolicy")
                .arg("Bypass")
                .arg("-File")
                .arg(&entry);
            c
        }
        other => bail!(
            "不支持的入口文件类型: .{other}（支持 msi / exe / bat / cmd / ps1）"
        ),
    };

    cmd.args(&meta.entry_args);
    cmd.current_dir(pkg_dir);
    // 不弹控制台窗口（与 rclone 子进程一致的处理）
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let status = cmd
        .status()
        .with_context(|| format!("启动入口文件失败: {}", entry.display()))?;
    Ok(status.code().unwrap_or(-1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pj(id: &str, ver: &str) -> PluginJson {
        PluginJson {
            id: id.into(),
            name: "T".into(),
            version: ver.into(),
            entry_file: "setup.msi".into(),
            ..Default::default()
        }
    }

    #[test]
    fn validate_accepts_good_manifest() {
        let mut p = pj("com.example.p", "1.0.4");
        p.min_gui_version = "2.0.0".into();
        p.platform = "windows".into();
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_rejects_missing_and_bad_fields() {
        let mut p = pj("", "1.0.0");
        assert!(p.validate().is_err(), "缺少 id 应报错");
        let mut p = pj("com.example.p", "");
        assert!(p.validate().is_err(), "缺少 version 应报错");
        let mut p = pj("com.example.p", "not-a-version");
        assert!(p.validate().is_err(), "非法版本应报错");
        let mut p = pj("nodot", "1.0.0");
        assert!(p.validate().is_err(), "id 需为反向域名");
        let mut p = pj("com.example.p", "1.0.0");
        p.entry_file = String::new();
        assert!(p.validate().is_err(), "缺少 entry_file 应报错");
        let mut p = pj("com.example.p", "1.0.0");
        p.platform = "linux".into();
        assert!(p.validate().is_err(), "非 windows 平台应报错");
    }

    #[test]
    fn compatibility_checks_min_version() {
        let mut p = pj("com.example.p", "1.0.0");
        p.min_gui_version = "2.0.0".into();
        assert!(p.compatible_with("2.1.0").is_ok());
        assert!(p.compatible_with("2.0.0").is_ok());
        assert!(p.compatible_with("1.9.9").is_err());
        // 未声明则不限
        p.min_gui_version = String::new();
        assert!(p.compatible_with("0.1.0").is_ok());
    }

    #[test]
    fn parses_manifest_lines() {
        let text = "\
# 注释行
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 config.json
BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD  winfsp.msi

a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2 *bin/helper.exe
";
        let es = parse_manifest(text).unwrap();
        assert_eq!(es.len(), 3);
        assert_eq!(es[0].path, "config.json");
        // 哈希统一小写
        assert_eq!(es[1].sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // 二进制标记被去掉
        assert_eq!(es[2].path, "bin/helper.exe");
    }

    #[test]
    fn rejects_malformed_manifest() {
        assert!(parse_manifest("").is_err(), "空清单应报错");
        assert!(parse_manifest("# 只有注释\n").is_err());
        assert!(parse_manifest("deadbeef config.json").is_err(), "哈希长度不足");
        assert!(
            parse_manifest("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855").is_err(),
            "缺少路径"
        );
    }

    #[test]
    fn version_comparison_uses_semver() {
        // 字符串比较会误判 1.0.10 < 1.0.9，semver 必须正确
        assert!(is_newer("1.0.10", "1.0.9"));
        assert!(!is_newer("1.0.9", "1.0.10"));
        assert!(is_newer("2.0.0", "1.9.9"));
        assert!(!is_newer("1.0.0", "1.0.0"), "相同版本不替换");
        assert!(!is_newer("0.9.0", "1.0.0"), "低版本不替换");
        // 预发布版本低于正式版
        assert!(is_newer("1.0.0", "1.0.0-beta.1"));
        // 非法版本保守处理：不更新
        assert!(!is_newer("abc", "1.0.0"));
        assert!(!is_newer("1.0.0", "abc"));
    }

    #[test]
    fn verifies_dir_end_to_end() {
        let base = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("plugin-pkg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("com.example.demo");
        std::fs::create_dir_all(&dir).unwrap();

        let entry = b"fake msi bytes";
        std::fs::write(dir.join("setup.msi"), entry).unwrap();
        let cfg = b"{\"a\":1}";
        std::fs::write(dir.join("config.json"), cfg).unwrap();

        let mut p = pj("com.example.demo", "1.0.0");
        p.entry_file = "setup.msi".into();
        std::fs::write(dir.join("plugin.json"), serde_json::to_vec(&p).unwrap()).unwrap();
        let mf = format!(
            "{}  setup.msi\n{}  config.json\n",
            zip::sha256_hex(entry),
            zip::sha256_hex(cfg)
        );
        std::fs::write(dir.join("manifest.sha256"), mf).unwrap();

        // 完整 → 通过
        let (meta, rep) = load_from_dir(&dir, "2.0.0").unwrap();
        assert_eq!(meta.id, "com.example.demo");
        assert_eq!(rep.checked, 2);

        // 篡改入口文件 → 必须被发现
        std::fs::write(dir.join("setup.msi"), b"TAMPERED").unwrap();
        let err = load_from_dir(&dir, "2.0.0").unwrap_err().to_string();
        assert!(err.contains("校验失败"), "应报完整性失败，实际: {err}");

        // 还原后删除清单中的文件 → 必须报缺失
        std::fs::write(dir.join("setup.msi"), entry).unwrap();
        std::fs::remove_file(dir.join("config.json")).unwrap();
        let err = load_from_dir(&dir, "2.0.0").unwrap_err().to_string();
        assert!(err.contains("缺少"), "应报缺少文件，实际: {err}");

        // 入口文件不在清单里 → 拒绝
        std::fs::write(dir.join("config.json"), cfg).unwrap();
        std::fs::write(dir.join("manifest.sha256"), format!("{}  config.json\n", zip::sha256_hex(cfg))).unwrap();
        let err = load_from_dir(&dir, "2.0.0").unwrap_err().to_string();
        assert!(err.contains("入口文件"), "应报入口文件未列入清单，实际: {err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 构造一个校验通过的插件目录
    fn make_pkg(base: &Path, id: &str, ver: &str, entry_bytes: &[u8]) -> PathBuf {
        let dir = base.join(format!("src-{}", sanitize_dir_name(id)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = pj(id, ver);
        p.entry_file = "setup.msi".into();
        std::fs::write(dir.join("setup.msi"), entry_bytes).unwrap();
        std::fs::write(dir.join("plugin.json"), serde_json::to_vec(&p).unwrap()).unwrap();
        std::fs::write(
            dir.join("manifest.sha256"),
            format!("{}  setup.msi\n", zip::sha256_hex(entry_bytes)),
        )
        .unwrap();
        dir
    }

    fn tmp_base(tag: &str) -> PathBuf {
        let base = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("plugin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn installs_new_plugin_then_skips_same_version() {
        let base = tmp_base("install1");
        let root = base.join("plugins");
        let src = make_pkg(&base, "com.example.a", "1.0.0", b"v1");

        let r = install_verified_dir(&src, &root, "2.0.0").unwrap();
        assert_eq!(
            r,
            InstallOutcome::Added {
                id: "com.example.a".into(),
                version: "1.0.0".into()
            }
        );
        assert!(root.join("com.example.a/plugin.json").is_file());

        // 相同版本 → 不替换
        let r2 = install_verified_dir(&src, &root, "2.0.0").unwrap();
        assert!(matches!(r2, InstallOutcome::Kept { .. }), "实际: {r2:?}");

        // 低版本 → 不替换
        let older = make_pkg(&base, "com.example.a", "0.9.0", b"v0");
        let r3 = install_verified_dir(&older, &root, "2.0.0").unwrap();
        assert!(matches!(r3, InstallOutcome::Kept { .. }), "实际: {r3:?}");
        assert_eq!(std::fs::read(root.join("com.example.a/setup.msi")).unwrap(), b"v1");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn upgrades_when_version_is_newer() {
        let base = tmp_base("install2");
        let root = base.join("plugins");
        let v1 = make_pkg(&base, "com.example.b", "1.0.0", b"old");
        install_verified_dir(&v1, &root, "2.0.0").unwrap();

        let v2 = make_pkg(&base, "com.example.b", "1.0.10", b"new");
        let r = install_verified_dir(&v2, &root, "2.0.0").unwrap();
        assert_eq!(
            r,
            InstallOutcome::Upgraded {
                id: "com.example.b".into(),
                from: "1.0.0".into(),
                to: "1.0.10".into()
            }
        );
        // 内容确实被替换
        assert_eq!(std::fs::read(root.join("com.example.b/setup.msi")).unwrap(), b"new");
        // 备份目录已清理
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".old-"))
            .collect();
        assert!(leftovers.is_empty(), "替换后不应残留备份目录");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_source_size_out_of_range() {
        assert!(check_source_size(10).is_err(), "过小应拒绝");
        assert!(check_source_size(MAX_PACKAGE_BYTES + 1).is_err(), "过大应拒绝");
        assert!(check_source_size(4096).is_ok(), "正常大小应通过");
    }

    #[test]
    fn sanitizes_plugin_id_for_directory_name() {
        assert_eq!(sanitize_dir_name("com.example.a"), "com.example.a");
        assert_eq!(sanitize_dir_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_dir_name("../evil"), ".._evil");
        // 不含路径分隔符，因此无法逃出插件根目录
        assert!(!sanitize_dir_name("../../x").contains('/'));
        assert!(!sanitize_dir_name("../../x").contains('\\'));
    }

    #[test]
    fn installs_from_zip_bytes() {
        let base = tmp_base("install3");
        let root = base.join("plugins");
        let tmp = base.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();

        // 构造一个完整插件包（含 manifest.sha256）的 zip
        let pj_json = br#"{"id":"com.example.z","name":"Z","version":"2.1.0","entry_file":"setup.msi"}"#;
        let msi = b"msi-bytes-here";
        let manifest = format!("{}  setup.msi\n", zip::sha256_hex(msi));
        let zip_bytes = crate::zip::tests::build_store_zip(&[
            ("plugin.json", pj_json.as_slice()),
            ("setup.msi", msi.as_slice()),
            ("manifest.sha256", manifest.as_bytes()),
        ]);

        let r = install_from_zip(&zip_bytes, &root, &tmp, "2.0.0").unwrap();
        assert_eq!(
            r,
            InstallOutcome::Added {
                id: "com.example.z".into(),
                version: "2.1.0".into()
            }
        );
        assert_eq!(std::fs::read(root.join("com.example.z/setup.msi")).unwrap(), msi);
        // 安装后重新扫描必须能通过完整性校验
        let (ok, bad) = scan_dir(&root, "2.0.0");
        assert_eq!(ok.len(), 1);
        assert!(bad.is_empty(), "安装结果应通过校验: {bad:?}");
        // 临时目录已清理
        let leftovers: Vec<_> = std::fs::read_dir(&tmp).unwrap().flatten().collect();
        assert!(leftovers.is_empty(), "临时目录应被清理: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// zip 内缺 manifest.sha256 → 必须拒绝（无法校验完整性）
    #[test]
    fn rejects_zip_without_manifest() {
        let base = tmp_base("install5");
        let root = base.join("plugins");
        let tmp = base.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();

        let pj_json = br#"{"id":"com.example.nomani","name":"N","version":"1.0.0","entry_file":"setup.msi"}"#;
        let zip_bytes = crate::zip::tests::build_store_zip(&[
            ("plugin.json", pj_json.as_slice()),
            ("setup.msi", b"x".as_slice()),
        ]);
        let err = install_from_zip(&zip_bytes, &root, &tmp, "2.0.0").unwrap_err().to_string();
        assert!(
            err.contains("manifest.sha256"),
            "应提示缺少 manifest.sha256，实际: {err}"
        );
        assert!(!root.join("com.example.nomani").exists(), "不应留下半成品");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_zip_without_plugin_json() {
        let base = tmp_base("install4");
        let root = base.join("plugins");
        let tmp = base.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();

        // 内容需大于最小体积阈值，确保测的是"缺少 plugin.json"而不是"文件太小"
        let filler = vec![b'A'; (MIN_PACKAGE_BYTES as usize) + 100];
        let zip_bytes = crate::zip::tests::build_store_zip(&[(
            "readme.txt",
            filler.as_slice(),
        )]);
        assert!(
            zip_bytes.len() as u64 >= MIN_PACKAGE_BYTES,
            "测试用 zip 必须超过最小体积阈值"
        );

        let err = install_from_zip(&zip_bytes, &root, &tmp, "2.0.0").unwrap_err().to_string();
        assert!(err.contains("plugin.json"), "应提示缺少 plugin.json，实际: {err}");
        // 不应在插件目录留下任何东西
        assert!(!root.join("readme.txt").exists());
        assert!(!root.join("com.example").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 单层子目录的 zip（打包工具常见）应能正确安装
    #[test]
    fn installs_from_zip_with_top_level_dir() {
        let base = tmp_base("install6");
        let root = base.join("plugins");
        let tmp = base.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();

        let pj_json = br#"{"id":"com.example.nest","name":"N","version":"3.0.0","entry_file":"setup.msi"}"#;
        let msi = b"nested-msi";
        let manifest = format!("{}  setup.msi\n", zip::sha256_hex(msi));
        let zip_bytes = crate::zip::tests::build_store_zip(&[
            ("nest/plugin.json", pj_json.as_slice()),
            ("nest/setup.msi", msi.as_slice()),
            ("nest/manifest.sha256", manifest.as_bytes()),
        ]);

        let r = install_from_zip(&zip_bytes, &root, &tmp, "2.0.0").unwrap();
        assert_eq!(
            r,
            InstallOutcome::Added {
                id: "com.example.nest".into(),
                version: "3.0.0".into()
            }
        );
        assert!(root.join("com.example.nest/plugin.json").is_file());
        assert_eq!(std::fs::read(root.join("com.example.nest/setup.msi")).unwrap(), msi);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_dir_isolates_broken_plugins() {
        let base = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("plugin-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        // 一个正常插件
        let good = base.join("good");
        std::fs::create_dir_all(&good).unwrap();
        let mut p = pj("com.example.good", "1.0.0");
        p.entry_file = "setup.msi".into();
        std::fs::write(good.join("setup.msi"), b"x").unwrap();
        std::fs::write(good.join("plugin.json"), serde_json::to_vec(&p).unwrap()).unwrap();
        std::fs::write(
            good.join("manifest.sha256"),
            format!("{}  setup.msi\n", zip::sha256_hex(b"x")),
        )
        .unwrap();

        // 一个哈希错误的插件
        let bad = base.join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("setup.msi"), b"y").unwrap();
        std::fs::write(bad.join("plugin.json"), serde_json::to_vec(&p).unwrap()).unwrap();
        std::fs::write(
            bad.join("manifest.sha256"),
            format!("{}  setup.msi\n", zip::sha256_hex(b"WRONG")),
        )
        .unwrap();

        // 一个没有 plugin.json 的目录（应被忽略，不计入错误）
        std::fs::create_dir_all(base.join("not-a-plugin")).unwrap();

        let (ok, errs) = scan_dir(&base, "2.0.0");
        assert_eq!(ok.len(), 1, "应只识别出 1 个正常插件");
        assert_eq!(errs.len(), 1, "应报告 1 个损坏插件");

        let _ = std::fs::remove_dir_all(&base);
    }
}
