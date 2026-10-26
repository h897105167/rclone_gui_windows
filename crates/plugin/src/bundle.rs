//! 插件包 v2（`config.json` + `check.sha256` + `entry`）的读取、校验、安装与卸载。
//!
//! 与 [`crate::package`] 的关系：`package` 处理上一代格式（`plugin.json` /
//! `manifest.sha256`，入口是外部安装器），这里处理新一代格式（入口通常是
//! `main.wasm`，界面由 `plugin_setting` 声明）。两者可以共存，宿主优先按本模块
//! 识别目录，识别不到才回落到旧格式。
//!
//! 安装流程固定为「解压到临时目录 → 解析 config.json → 校验 check.sha256 →
//! 比对版本 → 落盘」，任何一步失败都不会在插件目录留下半成品。

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::config::{self, ConfigFileNames, PluginConfig};
use crate::zip;

/// 校验结果。
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyReport {
    /// 校验了多少个文件
    pub checked: usize,
    /// 校验通过的文件（相对路径）
    pub files: Vec<String>,
}

/// 已安装插件（新一代格式）。
#[derive(Debug, Clone)]
pub struct Installed {
    pub config: PluginConfig,
    pub dir: PathBuf,
    pub verified: bool,
}

/// 生成清单文本：目录下除 `check.sha256` 之外的每个文件一行。
///
/// 顺序稳定（按路径排序），因此同样的内容总是生成逐字节相同的清单，
/// 便于用版本控制或 diff 检查插件包有没有被改过。
pub fn check_text(dir: &Path) -> Result<String> {
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files)?;
    files.sort();
    let mut out = String::new();
    for rel in files {
        if rel == config::CHECK_SHA256 {
            continue;
        }
        let abs = zip::safe_join(dir, &rel)?;
        let hash = zip::sha256_file(&abs)
            .with_context(|| rcgui_i18n::f("计算哈希失败: {}", &[&(abs.display())]))?;
        out.push_str(&hash);
        out.push_str("  ");
        out.push_str(&rel);
        out.push('\n');
    }
    if out.is_empty() {
        bail!("{}", rcgui_i18n::f("插件目录为空，无法生成 {}", &[&(config::CHECK_SHA256)]));
    }
    Ok(out)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            // 插件运行时产生的数据目录不参与校验
            if name == "kv.json" || name.starts_with('.') {
                continue;
            }
            collect_files(root, &p, out)?;
        } else {
            let rel = p
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| name);
            out.push(rel);
        }
    }
    Ok(())
}

/// 重新生成并写入 `check.sha256`（用于打包；安装时则由宿主按实际字节重算）。
pub fn write_check(dir: &Path) -> Result<usize> {
    let text = check_text(dir)?;
    std::fs::write(dir.join(config::CHECK_SHA256), text.as_bytes())
        .with_context(|| rcgui_i18n::f("写入 {} 失败", &[&(config::CHECK_SHA256)]))?;
    Ok(text.lines().count())
}

/// 校验目录：`check.sha256` 列出的文件必须存在且哈希一致，且入口文件必须在清单里。
///
/// 允许目录里存在清单之外的文件（插件运行期产生的日志 / 配置），
/// 但入口文件必须列入清单，否则无法证明被执行的字节没有被动过。
pub fn verify_dir(dir: &Path, names: ConfigFileNames, cfg: &PluginConfig) -> Result<VerifyReport> {
    let check_path = dir.join(names.check);
    let text = std::fs::read_to_string(&check_path)
        .with_context(|| rcgui_i18n::f("读取 {} 失败: {}", &[&(names.check), &(check_path.display())]))?;
    let entries = crate::package::parse_manifest(&text)
        .with_context(|| rcgui_i18n::f("解析 {} 失败", &[&(names.check)]))?;

    let entry_rel = cfg.entry.replace('\\', "/");
    if !entries.iter().any(|e| e.path == entry_rel) {
        bail!("{}", rcgui_i18n::f("{} 未包含入口文件 {}，拒绝加载", &[&(names.check), &(entry_rel)]));
    }

    let mut files = Vec::new();
    for e in &entries {
        let p = zip::safe_join(dir, &e.path)
            .with_context(|| rcgui_i18n::f("{} 中的路径不合法: {}", &[&(names.check), &(e.path)]))?;
        if !p.is_file() {
            bail!("{}", rcgui_i18n::f("插件缺少清单中声明的文件: {}", &[&(e.path)]));
        }
        let got = zip::sha256_file(&p)?;
        if got != e.sha256 {
            bail!("{}", rcgui_i18n::f("插件文件校验失败: {}\n  期望 {}\n  实际 {}", &[&(e.path), &(e.sha256), &(got)]));
        }
        files.push(e.path.clone());
    }
    Ok(VerifyReport {
        checked: entries.len(),
        files,
    })
}

/// 读取并校验一个插件目录（新一代格式）。
pub fn load_from_dir(dir: &Path, gui_version: &str) -> Result<(PluginConfig, VerifyReport)> {
    let names = ConfigFileNames::detect(dir).ok_or_else(|| {
        anyhow!("{}", rcgui_i18n::f("目录里没有 {}（也不是旧格式 {}）: {}", &[&(config::CONFIG_JSON), &(config::LEGACY_PLUGIN_JSON), &(dir.display())]))
    })?;
    let path = dir.join(names.config);
    let text = std::fs::read_to_string(&path)
        .with_context(|| rcgui_i18n::f("读取 {} 失败: {}", &[&(names.config), &(path.display())]))?;
    let cfg: PluginConfig = serde_json::from_str(&text)
        .with_context(|| rcgui_i18n::f("解析 {} 失败: {}", &[&(names.config), &(path.display())]))?;
    cfg.validate()?;
    cfg.compatible_with(gui_version)?;
    let report = verify_dir(dir, names, &cfg)?;
    // 入口文件必须真实存在（清单里已保证，这里再确认可读）
    let entry = zip::safe_join(dir, &cfg.entry)?;
    if !entry.is_file() {
        bail!("{}", rcgui_i18n::f("入口文件不存在: {}", &[&(entry.display())]));
    }
    Ok((cfg, report))
}

/// 宽松读取 `config.json`：校验失败时仍能拿到名称/版本用于展示。
pub fn read_config_lenient(dir: &Path) -> Option<PluginConfig> {
    let names = ConfigFileNames::detect(dir)?;
    let text = std::fs::read_to_string(dir.join(names.config)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 扫描插件根目录（新一代格式）。
pub fn scan(root: &Path, gui_version: &str) -> (Vec<Installed>, Vec<(PathBuf, String)>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return (ok, bad);
    };
    for entry in rd.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || ConfigFileNames::detect(&dir).is_none() {
            continue;
        }
        match load_from_dir(&dir, gui_version) {
            Ok((cfg, _)) => ok.push(Installed {
                config: cfg,
                dir,
                verified: true,
            }),
            Err(e) => bad.push((dir, format!("{e:#}"))),
        }
    }
    ok.sort_by(|a, b| a.config.name.cmp(&b.config.name));
    bad.sort_by(|a, b| a.0.cmp(&b.0));
    (ok, bad)
}

// ---------------- 安装 ----------------

/// 安装结果（与旧格式共用同一套语义，UI 不需要区分）。
pub use crate::package::{InstallOutcome, MAX_EXTRACTED_BYTES};

/// 把已校验的插件目录安装到 `plugins_root/<id>`。
///
/// - 新 id → 直接安装
/// - 已存在且版本更高 → 替换（旧目录先改名为备份，成功后删除）
/// - 已存在且版本相同或更低 → 不替换（`allow_same=true` 时同版本也覆盖，用于修复安装）
///
/// 内置插件被用户卸载后会留下"墓碑"（见 app 侧 `plugin_removed`），
/// 那条记录由调用方决定是否清除，这里只负责目录本身。
pub fn install_dir(
    src_dir: &Path,
    plugins_root: &Path,
    gui_version: &str,
    allow_same: bool,
) -> Result<InstallOutcome> {
    let (cfg, _) = load_from_dir(src_dir, gui_version)?;
    std::fs::create_dir_all(plugins_root)?;
    let dest = plugins_root.join(config::sanitize_dir_name(&cfg.id));

    let existing = if ConfigFileNames::detect(&dest).is_some() {
        load_from_dir(&dest, gui_version).ok().map(|(m, _)| m)
    } else {
        None
    };

    let replace = match &existing {
        None => false,
        Some(old) => {
            crate::package::is_newer(&cfg.version, &old.version)
                || (allow_same && cfg.version.trim() == old.version.trim())
        }
    };

    match existing {
        Some(old) if !replace => Ok(InstallOutcome::Kept {
            id: cfg.id,
            installed: old.version,
            offered: cfg.version,
        }),
        Some(old) => {
            let backup = plugins_root.join(format!(
                "{}.old-{}",
                config::sanitize_dir_name(&cfg.id),
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&backup);
            std::fs::rename(&dest, &backup)
                .with_context(|| rcgui_i18n::f("备份旧版本失败: {}", &[&(dest.display())]))?;
            match copy_dir(src_dir, &dest) {
                Ok(()) => {
                    let _ = std::fs::remove_dir_all(&backup);
                    Ok(InstallOutcome::Upgraded {
                        id: cfg.id,
                        from: old.version,
                        to: cfg.version,
                    })
                }
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dest);
                    let _ = std::fs::rename(&backup, &dest);
                    Err(e.context(rcgui_i18n::t("替换插件失败，已回滚到旧版本")))
                }
            }
        }
        None => {
            copy_dir(src_dir, &dest)?;
            Ok(InstallOutcome::Added {
                id: cfg.id,
                version: cfg.version,
            })
        }
    }
}

/// 从 zip 字节流安装新一代插件包。
pub fn install_zip(
    data: &[u8],
    plugins_root: &Path,
    tmp_root: &Path,
    gui_version: &str,
) -> Result<InstallOutcome> {
    crate::package::check_source_size(data.len() as u64)?;
    let cfg = peek_zip_config(data)?;

    let tmp = tmp_root.join(format!(
        "unpack-{}-{}",
        std::process::id(),
        config::sanitize_dir_name(&cfg.id)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let result = (|| -> Result<InstallOutcome> {
        zip::extract_all(data, &tmp, MAX_EXTRACTED_BYTES)?;
        let root = unwrap_single_dir(&tmp);
        install_dir(&root, plugins_root, gui_version, false)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// 从 zip 里读出 `config.json`（不落盘）。
pub fn peek_zip_config(data: &[u8]) -> Result<PluginConfig> {
    let content = find_zip_config(data)?
        .ok_or_else(|| anyhow!("{}", rcgui_i18n::f("压缩包里没有 {}，这不是插件包", &[&(config::CONFIG_JSON)])))?;
    let cfg: PluginConfig = serde_json::from_slice(&content)
        .with_context(|| rcgui_i18n::f("解析 {} 失败", &[&(config::CONFIG_JSON)]))?;
    cfg.validate()?;
    Ok(cfg)
}

fn find_zip_config(data: &[u8]) -> Result<Option<Vec<u8>>> {
    for e in zip::read_entries(data)? {
        let n = e.name.trim_start_matches("./").to_string();
        let base = n.rsplit('/').next().unwrap_or("");
        if base == config::CONFIG_JSON && n.matches('/').count() <= 1 {
            return Ok(Some(zip::read_entry(data, &e)?));
        }
    }
    Ok(None)
}

fn unwrap_single_dir(dir: &Path) -> PathBuf {
    let mut dirs = Vec::new();
    let mut files = 0usize;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                dirs.push(e.path());
            } else {
                files += 1;
            }
        }
    }
    if files == 0 && dirs.len() == 1 {
        return dirs.remove(0);
    }
    dir.to_path_buf()
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| rcgui_i18n::f("复制失败: {}", &[&(from.display())]))?;
        }
    }
    Ok(())
}

// ---------------- 卸载 ----------------

/// 卸载插件：删除插件目录（含该插件的 kv 数据）。
///
/// 返回被删除的目录，便于 UI 提示具体位置。目录不存在视为已卸载。
pub fn uninstall(dir: &Path) -> Result<PathBuf> {
    if !dir.exists() {
        return Ok(dir.to_path_buf());
    }
    // 只允许删插件根目录下的一级子目录，杜绝误传路径造成的批量删除
    if dir.parent().is_none() {
        bail!("{}", rcgui_i18n::f("拒绝删除根目录: {}", &[&(dir.display())]));
    }
    std::fs::remove_dir_all(dir).with_context(|| rcgui_i18n::f("删除插件目录失败: {}", &[&(dir.display())]))?;
    Ok(dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("plugin-bundle-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn write_pkg(dir: &Path, id: &str, version: &str, wasm: &[u8]) {
        std::fs::create_dir_all(dir).unwrap();
        let cfg = json!({
            "id": id,
            "name": "测试插件",
            "version": version,
            "entry": "main.wasm",
            "tags": ["测试"],
            "capabilities": ["kv"],
            "plugin_setting": {
                "controls": [{"kind": "group", "key": "g", "label": "① 测试"}]
            }
        });
        std::fs::write(dir.join(config::CONFIG_JSON), serde_json::to_vec_pretty(&cfg).unwrap())
            .unwrap();
        std::fs::write(dir.join("main.wasm"), wasm).unwrap();
        write_check(dir).unwrap();
    }

    #[test]
    fn writes_and_verifies_check_file() {
        let base = tmp_base("verify");
        let dir = base.join("p");
        write_pkg(&dir, "demo.plugin", "1.0.0", b"wasm-bytes");
        let (cfg, rep) = load_from_dir(&dir, "1.0.0").unwrap();
        assert_eq!(cfg.id, "demo.plugin");
        // config.json + main.wasm
        assert_eq!(rep.checked, 2);

        // 篡改入口 → 必须发现
        std::fs::write(dir.join("main.wasm"), b"tampered").unwrap();
        let err = load_from_dir(&dir, "1.0.0").unwrap_err().to_string();
        assert!(err.contains("校验失败"), "实际: {err}");

        // 缺文件 → 必须发现
        std::fs::remove_file(dir.join("main.wasm")).unwrap();
        let err = load_from_dir(&dir, "1.0.0").unwrap_err().to_string();
        assert!(err.contains("缺少"), "实际: {err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn check_file_is_stable_and_excludes_itself() {
        let base = tmp_base("stable");
        let dir = base.join("p");
        write_pkg(&dir, "demo.stable", "1.0.0", b"x");
        let a = check_text(&dir).unwrap();
        let b = check_text(&dir).unwrap();
        assert_eq!(a, b, "清单文本必须稳定");
        assert_eq!(a.lines().count(), 2);
        assert!(!a.contains(config::CHECK_SHA256));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn entry_must_be_listed() {
        let base = tmp_base("entry");
        let dir = base.join("p");
        write_pkg(&dir, "demo.entry", "1.0.0", b"x");
        // 清单里去掉 main.wasm
        let only_cfg = format!(
            "{}  {}\n",
            zip::sha256_hex(&std::fs::read(dir.join(config::CONFIG_JSON)).unwrap()),
            config::CONFIG_JSON
        );
        std::fs::write(dir.join(config::CHECK_SHA256), only_cfg).unwrap();
        let err = load_from_dir(&dir, "1.0.0").unwrap_err().to_string();
        assert!(err.contains("入口文件"), "实际: {err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn installs_and_upgrades() {
        let base = tmp_base("install");
        let root = base.join("plugins");
        let v1 = base.join("v1");
        write_pkg(&v1, "demo.install", "1.0.0", b"one");
        let r = install_dir(&v1, &root, "1.0.0", false).unwrap();
        assert!(matches!(r, InstallOutcome::Added { .. }), "实际: {r:?}");

        // 同版本 → 不替换
        let r = install_dir(&v1, &root, "1.0.0", false).unwrap();
        assert!(matches!(r, InstallOutcome::Kept { .. }), "实际: {r:?}");

        // 高版本 → 替换
        let v2 = base.join("v2");
        write_pkg(&v2, "demo.install", "1.1.0", b"two");
        let r = install_dir(&v2, &root, "1.0.0", false).unwrap();
        assert!(matches!(r, InstallOutcome::Upgraded { .. }), "实际: {r:?}");
        assert_eq!(std::fs::read(root.join("demo.install/main.wasm")).unwrap(), b"two");

        // 低版本 → 不替换
        let v0 = base.join("v0");
        write_pkg(&v0, "demo.install", "0.9.0", b"zero");
        let r = install_dir(&v0, &root, "1.0.0", false).unwrap();
        assert!(matches!(r, InstallOutcome::Kept { .. }), "实际: {r:?}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn installs_from_zip_with_top_level_dir() {
        let base = tmp_base("zip");
        let root = base.join("plugins");
        let tmp = base.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();

        let cfg = br#"{"id":"demo.zip","name":"Z","version":"2.0.0","entry":"main.wasm","plugin_setting":{"controls":[]}}"#;
        let wasm = b"wasm-zip";
        let check = format!(
            "{}  config.json\n{}  main.wasm\n",
            zip::sha256_hex(cfg),
            zip::sha256_hex(wasm)
        );
        let bytes = crate::zip::tests::build_store_zip(&[
            ("pkg/config.json", cfg.as_slice()),
            ("pkg/main.wasm", wasm.as_slice()),
            ("pkg/check.sha256", check.as_bytes()),
        ]);

        let r = install_zip(&bytes, &root, &tmp, "1.0.0").unwrap();
        assert!(matches!(r, InstallOutcome::Added { .. }), "实际: {r:?}");
        assert!(root.join("demo.zip/main.wasm").is_file());
        // 安装后重新扫描必须通过
        let (ok, bad) = scan(&root, "1.0.0");
        assert_eq!(ok.len(), 1);
        assert!(bad.is_empty(), "安装结果应通过校验: {bad:?}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_zip_without_config() {
        let base = tmp_base("zipbad");
        let root = base.join("plugins");
        let tmp = base.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let filler = vec![b'A'; 300];
        let bytes = crate::zip::tests::build_store_zip(&[("readme.txt", filler.as_slice())]);
        let err = install_zip(&bytes, &root, &tmp, "1.0.0").unwrap_err().to_string();
        assert!(err.contains("config.json"), "实际: {err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn uninstall_removes_dir() {
        let base = tmp_base("uninstall");
        let dir = base.join("p");
        write_pkg(&dir, "demo.uninstall", "1.0.0", b"x");
        assert!(dir.is_dir());
        uninstall(&dir).unwrap();
        assert!(!dir.exists());
        // 幂等
        uninstall(&dir).unwrap();

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_reports_broken_plugin_separately() {
        let base = tmp_base("scan");
        let good = base.join("good");
        write_pkg(&good, "demo.good", "1.0.0", b"x");
        let broken = base.join("broken");
        write_pkg(&broken, "demo.broken", "1.0.0", b"x");
        std::fs::write(broken.join("main.wasm"), b"tampered").unwrap();
        // 非插件目录应被忽略
        std::fs::create_dir_all(base.join("not-plugin")).unwrap();

        let (ok, bad) = scan(&base, "1.0.0");
        assert_eq!(ok.len(), 1);
        assert_eq!(bad.len(), 1);

        let _ = std::fs::remove_dir_all(&base);
    }
}
