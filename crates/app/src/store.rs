//! 插件商店：读 HTTPS 清单、校验插件包、安装与更新。
//!
//! 这里每个校验都刻意做成"先验证、后落盘"。旧的目录只有在完整包通过
//! 校验之后才会被替换，替换过程中的任何一步失败都会把旧目录挪回来
//! （`plugin_host::bundle::install_zip` 内部已经是这个顺序）。
//!
//! 商店是**不可信输入**：清单在别人控制的静态站点上，包在别人控制的
//! 仓库里。因此：
//!
//! * 清单只接受 `https://`（本机回环地址例外，方便开发时起静态服务）；
//! * 包必须与清单里的 `sha256` 一致，且包内 `id`/`version` 与清单一致；
//! * 包必须满足 `min_gui_version`，否则装了也跑不起来；
//! * 权限只展示、不授予 —— 安装后插件仍是禁用状态，由用户去插件页启用。
//!
//! 一个包能在商店里出现的唯一凭据就是"清单里写了它的哈希"。

use crate::core;
use anyhow::{bail, Context as _, Result};
use std::path::{Path, PathBuf};

/// 默认清单地址。
pub const DEFAULT_STORE_URL: &str =
    "https://h897105167.github.io/rclone_gui_windows/store/index.json";
/// 清单体积上限。清单是纯文本元数据，512 KB 足够列几百个插件。
pub const MAX_INDEX_BYTES: u64 = 512 * 1024;
/// 单个插件包的体积上限（与本地安装同一套限制）。
pub const MAX_PACKAGE_BYTES: u64 = plugin_host::package::MAX_PACKAGE_BYTES;
/// 清单里最多接受多少个插件条目。
pub const MAX_ENTRIES: usize = 200;
/// 商店里永远不出现的插件。
///
/// oneclick 的历史职责（拉起/停止 rclone 进程）已经并入应用自己的内核管理
/// （见 `crate::core`）。如果它还能从商店装回来，用户会遇到两个进程管理同一个
/// 配置目录的情况。
pub const FORBIDDEN_IDS: &[&str] = &["oneclick", "rcgui-plugin-oneclick", "onerclone"];

/// 清单里的一个插件。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoreEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// 源码仓库地址，仅用于展示。
    #[serde(default)]
    pub repository: String,
    /// 包下载地址。
    pub url: String,
    /// 包的 SHA256（小写十六进制）。
    pub sha256: String,
    /// 要求的最低 GUI 版本；空表示不限制。
    #[serde(default)]
    pub min_gui_version: String,
    /// 声明的能力组，安装前给用户看。
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl StoreEntry {
    /// 校验这条清单项本身是否可用。错误文案直接展示给用户。
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("清单里有一条插件缺少 id");
        }
        if !self.id.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || c == '.'
        }) {
            bail!(
                "插件 id「{}」含有不允许的字符（只允许小写字母、数字、- _ .）",
                self.id
            );
        }
        if FORBIDDEN_IDS.contains(&self.id.as_str()) {
            bail!(
                "插件 {} 不允许通过商店安装（其职能已并入应用内置的内核管理）",
                self.id
            );
        }
        if self.version.trim().is_empty() {
            bail!("插件 {} 缺少版本号", self.id);
        }
        check_url(&self.url).with_context(|| format!("插件 {} 的下载地址不可用", self.id))?;
        let h = self.sha256.trim().to_ascii_lowercase();
        if h.len() != 64 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!(
                "插件 {} 的 sha256 格式不正确（需要 64 位十六进制）",
                self.id
            );
        }
        if !self.min_gui_version.trim().is_empty() && parse_version(&self.min_gui_version).is_none()
        {
            bail!(
                "插件 {} 的 min_gui_version「{}」不是有效的版本号",
                self.id,
                self.min_gui_version
            );
        }
        Ok(())
    }

    /// 这个包能不能装在这个 GUI 版本上。
    pub fn allows_gui(&self, gui_version: &str) -> Result<()> {
        let min = self.min_gui_version.trim();
        if min.is_empty() {
            return Ok(());
        }
        let (Some(min), Some(cur)) = (parse_version(min), parse_version(gui_version)) else {
            // 版本号解析不出来时不放行：宁可让用户手动装，也不要在不确定的情况下
            // 装一个可能依赖未来 API 的插件。
            bail!(
                "无法比较版本（插件要求 ≥{}，当前 {}），已中止安装",
                self.min_gui_version,
                gui_version
            );
        };
        if cur < min {
            bail!(
                "插件 {} 需要 GUI ≥ {}，当前是 {}：请先升级本程序",
                self.id,
                self.min_gui_version,
                gui_version
            );
        }
        Ok(())
    }

    /// 安装前给用户看的权限清单，一条一项，已排序去重。空表表示没有声明权限。
    ///
    /// 之所以给列表而不是拼好的字符串：界面要一项一行地排版换行，读屏也要
    /// 逐项念，把换行藏在字符串里两件事都做不到。
    pub fn permissions_list(&self) -> Vec<String> {
        if self.capabilities.is_empty() {
            return Vec::new();
        }
        let mut caps = self.capabilities.clone();
        caps.sort();
        caps.dedup();
        caps.iter().map(|c| capability_line(c)).collect()
    }

    /// 同上，拼成一行一条的纯文本。给只能放一段文字的确认框用。
    pub fn permissions_text(&self) -> String {
        self.permissions_list().join("\n")
    }
}

/// 清单文件。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StoreIndex {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub plugins: Vec<StoreEntry>,
}

/// 读回来的清单 + 被丢掉的条目（用于在状态栏里解释"为什么少了一个"）。
#[derive(Debug, Clone, Default)]
pub struct ParsedIndex {
    pub entries: Vec<StoreEntry>,
    pub warnings: Vec<String>,
}

/// 解析并校验清单文本。
///
/// 单条坏数据不应该让整个商店打不开：坏条目被丢掉并记一条 warning，
/// 但 `schema_version` 不认识、或者一条有效条目都没有，仍然当失败处理
/// —— 那说明清单整体结构变了，继续展示空列表会让人以为"商店里什么都没有"。
pub fn parse_index(text: &str) -> Result<ParsedIndex> {
    let raw: StoreIndex =
        serde_json::from_str(text).with_context(|| rcgui_i18n::t("清单不是合法的 JSON"))?;
    if raw.schema_version != 1 {
        bail!(
            "清单的 schema_version 是 {}，本程序只认识 1：请升级程序或换一个清单地址",
            raw.schema_version
        );
    }
    if raw.plugins.len() > MAX_ENTRIES {
        bail!(
            "清单含 {} 个插件，超过上限 {}",
            raw.plugins.len(),
            MAX_ENTRIES
        );
    }
    let mut out = ParsedIndex::default();
    let mut seen = std::collections::HashSet::new();
    for e in raw.plugins {
        match e.validate() {
            Ok(()) => {
                if !seen.insert(e.id.to_ascii_lowercase()) {
                    out.warnings.push(rcgui_i18n::f(
                        "清单里 {} 出现了多次，只保留第一条",
                        &[&(e.id)],
                    ));
                    continue;
                }
                out.entries.push(e);
            }
            Err(err) => out.warnings.push(rcgui_i18n::f(
                "已跳过清单里的一条插件：{}",
                &[&(format!("{err:#}"))],
            )),
        }
    }
    if out.entries.is_empty() && !out.warnings.is_empty() {
        bail!("清单里没有一条可用的插件（{}）", out.warnings.join("；"));
    }
    Ok(out)
}

/// 下载并解析清单。`url` 为空时用默认地址。
pub async fn fetch_index(url: &str) -> Result<ParsedIndex> {
    let url = if url.trim().is_empty() {
        DEFAULT_STORE_URL
    } else {
        url.trim()
    };
    check_url(url).context("商店地址不可用")?;
    let bytes = core::fetch_bytes(url, MAX_INDEX_BYTES)
        .await
        .with_context(|| rcgui_i18n::f("下载商店清单失败: {}", &[&(url)]))?;
    let text = String::from_utf8(bytes).context(rcgui_i18n::t("清单不是 UTF-8 文本"))?;
    parse_index(&text)
}

/// 只允许 HTTPS；本机回环地址允许 HTTP，用于本地起静态站点调试。
pub fn check_url(url: &str) -> Result<()> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    if lower.starts_with("https://") && u.len() > 8 {
        return Ok(());
    }
    if let Some(rest) = lower.strip_prefix("http://") {
        let host = rest.split(['/', ':', '?']).next().unwrap_or("");
        let loopback = host == "127.0.0.1" || host == "localhost" || host == "[::1]";
        if loopback {
            return Ok(());
        }
        bail!("只接受 HTTPS 地址（{} 是本机回环之外的明文 HTTP）", url);
    }
    bail!("只接受 https:// 开头的地址（当前：{}）", url);
}

/// `1.2.3` → `(1,2,3)`；预发布后缀（`-beta.1`）只取主体部分。
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or("");
    let mut it = core.split('.');
    let a = it.next()?.parse::<u64>().ok()?;
    let b = it.next().unwrap_or("0").parse::<u64>().unwrap_or(0);
    let c = it.next().unwrap_or("0").parse::<u64>().unwrap_or(0);
    Some((a, b, c))
}

/// 能力组 → 一句人话。未知能力原样展示（并标出来），不假装它无害。
///
/// 这八个组以 `plugin_host::config::CAP_GROUPS` 为准：清单里写了别的字符串
/// 就是清单错了，照实说"未识别的权限"，别给它编一句好听的话盖过去。
fn capability_line(cap: &str) -> String {
    match cap {
        "kv" => rcgui_i18n::t("读写本应用内的键值存储").to_string(),
        "ui" => rcgui_i18n::t("往界面里弹提示、加自己的控件").to_string(),
        "fs" => rcgui_i18n::t("读写本机文件（选文件、解压、算哈希）").to_string(),
        "http" => rcgui_i18n::t("访问网络（按你填写的地址收发请求）").to_string(),
        "proc" => rcgui_i18n::t("启动、停止、查询其他程序").to_string(),
        "net" => rcgui_i18n::t("探测端口是否开放").to_string(),
        "rclone" => rcgui_i18n::t("通过已连接的 rclone 读写远程").to_string(),
        "env" => rcgui_i18n::t("读取本应用的版本、架构、是否提权").to_string(),
        other => rcgui_i18n::f("未识别的权限：{}", &[&(other)]),
    }
}

/// 校验通过的包信息：装之前从包里读出来的真实身份。
#[derive(Debug, Clone, PartialEq)]
pub struct PackageMeta {
    pub id: String,
    pub version: String,
}

/// 从 zip 字节里读出包身份（不解压全部）。
///
/// 新格式看 `config.json`，旧格式看 `plugin.json`；两种都读不到就是"不是插件包"。
pub fn peek_meta(data: &[u8]) -> Result<PackageMeta> {
    if let Ok(cfg) = plugin_host::bundle::peek_zip_config(data) {
        return Ok(PackageMeta {
            id: cfg.id,
            version: cfg.version,
        });
    }
    let pj = plugin_host::package::peek_zip_meta(data).context(rcgui_i18n::t(
        "这个包里既没有 config.json 也没有 plugin.json，不是插件包",
    ))?;
    Ok(PackageMeta {
        id: pj.id,
        version: pj.version,
    })
}

/// 校验「下载到的字节」确实就是清单说的那个包。
///
/// 顺序不能换：哈希 → 身份 → 版本 → GUI 兼容性。哈希排第一，因为它是唯一
/// 能证明"这份字节就是清单作者发布的那份"的检查；后面几项只防"清单写错了"。
pub fn verify_package(entry: &StoreEntry, data: &[u8], gui_version: &str) -> Result<PackageMeta> {
    entry.validate()?;
    if data.len() as u64 > MAX_PACKAGE_BYTES {
        bail!(
            "插件包 {} 字节，超过上限 {} 字节",
            data.len(),
            MAX_PACKAGE_BYTES
        );
    }
    let want = entry.sha256.trim().to_ascii_lowercase();
    let got = core::sha256_hex(data);
    if got != want {
        bail!(
            "插件 {} 的 sha256 不匹配（清单 {}，实际 {}）：包可能被改动过，已中止",
            entry.id,
            want,
            got
        );
    }
    let meta = peek_meta(data)?;
    if !meta.id.eq_ignore_ascii_case(&entry.id) {
        bail!(
            "包的 id 是「{}」，与清单里的「{}」不一致，已中止",
            meta.id,
            entry.id
        );
    }
    if meta.version.trim() != entry.version.trim() {
        bail!(
            "插件 {} 的包内版本 {} 与清单写的 {} 不一致，已中止",
            entry.id,
            meta.version,
            entry.version
        );
    }
    entry.allows_gui(gui_version)?;
    Ok(meta)
}

/// 下载并安装（或更新）一个插件。
///
/// `plugins_root` 是插件根目录，`tmp_root` 用于解压暂存。安装走与本地安装
/// 完全相同的代码路径（新格式优先，旧格式回落），因此"替换失败自动回滚"
/// 这一条不需要在这里重复实现。
///
/// 安装成功**不会**自动启用插件：启用是用户在插件页里的动作，因为启用等于
/// 授予清单里声明的全部权限。
pub async fn install(
    entry: &StoreEntry,
    plugins_root: &Path,
    tmp_root: &Path,
    gui_version: &str,
) -> Result<plugin_host::package::InstallOutcome> {
    entry.validate()?;
    entry.allows_gui(gui_version)?;
    let data = core::fetch_bytes(&entry.url, MAX_PACKAGE_BYTES)
        .await
        .with_context(|| rcgui_i18n::f("下载插件包失败: {}", &[&(entry.id)]))?;
    // 校验在落盘之前完成；verify_package 用的是下载到的字节，不是清单里抄的数字。
    verify_package(entry, &data, gui_version)?;

    let root = plugins_root.to_path_buf();
    let tmp = tmp_root.to_path_buf();
    let gv = gui_version.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        match plugin_host::bundle::install_zip(&data, &root, &tmp, &gv) {
            Ok(o) => Ok(o),
            Err(new_err) => match plugin_host::package::install_from_zip(&data, &root, &tmp, &gv) {
                Ok(o) => Ok(o),
                Err(_) => Err(new_err),
            },
        }
    })
    .await
    .context(rcgui_i18n::t("安装线程异常"))??;

    // 用户卸载过这个插件、现在又从商店装了回来：把卸载时备份的设置放回原位。
    // 只补空位，自己带了 kv.json 或已经重新填过的新值都不动。
    crate::plugins::restore_backup_after_install(&outcome);

    // 记下"这个插件是从商店装的"。导出配置时靠它区分来源（内置 / 商店 / 用户自己放的），
    // 导入端才能知道该不该去商店重装。
    //
    // 每次装完都要重写：升级走的替换分支只保留 `kv.json`，origin.json 属于包内文件，
    // 会被新包的表单覆盖或随旧目录一起丢掉。写失败只影响导出时的来源标注，
    // 已经装好的插件不该因为写不了这一行而被判失败，所以这里不做错误处理。
    if let Some(dir) = dir_of(plugins_root, &entry.id) {
        crate::plugins::write_origin(&dir, "store", &entry.url, &entry.sha256);
    }
    Ok(outcome)
}

/// 按插件 id 在插件根目录里找出它的实际安装目录名。
///
/// 目录名不完全等于 id（安装时会做字符清洗），而且旧格式插件是单个 `.wasm` 文件、
/// 没有目录 —— 这两种情况都只能靠读目录里的 config.json 来确认。
fn dir_of(plugins_root: &Path, id: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(plugins_root).ok()?;
    for e in rd.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        if let Some(cfg) = plugin_host::bundle::read_config_lenient(&dir) {
            if cfg.id.eq_ignore_ascii_case(id) {
                return Some(dir);
            }
        }
    }
    None
}

/// 已安装插件的最小信息，用于判断商店里的版本是否更新。
#[derive(Debug, Clone)]
pub struct Installed {
    pub id: String,
    pub version: String,
}

/// 扫出已安装插件（新格式与旧格式都算）。单个坏包不影响其余。
pub fn scan_installed(plugins_root: &Path, gui_version: &str) -> Vec<Installed> {
    let mut out: Vec<Installed> = Vec::new();
    let (new, _bad) = plugin_host::bundle::scan(plugins_root, gui_version);
    for p in new {
        out.push(Installed {
            id: p.config.id,
            version: p.config.version,
        });
    }
    let (old, _bad2) = plugin_host::package::scan_dir(plugins_root, gui_version);
    for p in old {
        if !out.iter().any(|x| x.id.eq_ignore_ascii_case(&p.meta.id)) {
            out.push(Installed {
                id: p.meta.id,
                version: p.meta.version,
            });
        }
    }
    out
}

/// 商店版本是否比已装版本新。已装版本读不出来时按"不更新"，避免用坏数据覆盖。
pub fn is_update(entry: &StoreEntry, installed: Option<&str>) -> bool {
    match installed {
        Some(v) => plugin_host::package::is_newer(&entry.version, v),
        None => false,
    }
}

/// 给界面用的 `StoreRow` 数据（不含 Slint 类型，便于测试）。
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub installed: bool,
    pub update: bool,
    pub permissions: Vec<String>,
}

/// 把清单与已安装列表合成界面行。
pub fn rows(entries: &[StoreEntry], installed: &[Installed]) -> Vec<Row> {
    entries
        .iter()
        .map(|e| {
            let cur = installed
                .iter()
                .find(|i| i.id.eq_ignore_ascii_case(&e.id))
                .map(|i| i.version.as_str());
            Row {
                id: e.id.clone(),
                name: if e.name.trim().is_empty() {
                    e.id.clone()
                } else {
                    e.name.clone()
                },
                version: e.version.clone(),
                description: e.description.clone(),
                installed: cur.is_some(),
                update: is_update(e, cur),
                permissions: e.permissions_list(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> StoreEntry {
        StoreEntry {
            id: "kvault".into(),
            name: "K-Vault".into(),
            version: "1.2.0".into(),
            description: "加密仓库".into(),
            repository: "https://github.com/x/y".into(),
            url: "https://example.com/kvault.zip".into(),
            sha256: "a".repeat(64),
            min_gui_version: "0.3.0".into(),
            capabilities: vec!["http".into(), "kv".into()],
        }
    }

    #[test]
    fn check_url_requires_https_except_loopback() {
        assert!(check_url("https://example.com/a.json").is_ok());
        assert!(check_url("http://127.0.0.1:8000/index.json").is_ok());
        assert!(check_url("http://localhost:8000/index.json").is_ok());
        assert!(check_url("http://example.com/index.json").is_err());
        assert!(check_url("file:///c:/a.json").is_err());
        assert!(check_url("https://").is_err());
    }

    #[test]
    fn validate_rejects_bad_sha_and_id() {
        let mut e = entry();
        assert!(e.validate().is_ok());

        e.sha256 = "abc".into();
        assert!(e.validate().is_err());

        e = entry();
        e.id = "a/b".into();
        assert!(e.validate().is_err());

        e = entry();
        e.url = "http://example.com/x.zip".into();
        assert!(e.validate().is_err());

        e = entry();
        e.min_gui_version = "newest".into();
        assert!(e.validate().is_err());
    }

    #[test]
    fn oneclick_cannot_come_back_from_store() {
        let mut e = entry();
        e.id = "oneclick".into();
        let err = e.validate().unwrap_err().to_string();
        assert!(err.contains("不允许"), "{err}");
    }

    #[test]
    fn index_skips_bad_entries_but_keeps_good_ones() {
        let text = r#"{
            "schema_version": 1,
            "plugins": [
                {"id":"kvault","version":"1.0.0","url":"https://e.com/a.zip","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                {"id":"broken","version":"1.0.0","url":"http://e.com/b.zip","sha256":"aaa"},
                {"id":"oneclick","version":"9.9.9","url":"https://e.com/c.zip","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
            ]
        }"#;
        let p = parse_index(text).unwrap();
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0].id, "kvault");
        assert_eq!(p.warnings.len(), 2);
    }

    #[test]
    fn index_rejects_unknown_schema_and_empty() {
        assert!(parse_index(r#"{"schema_version":2,"plugins":[]}"#).is_err());
        // 认识的 schema 但全坏 → 报错而不是展示空列表
        let all_bad = r#"{"schema_version":1,"plugins":[{"id":"x","version":"1","url":"ftp://e/a.zip","sha256":"a"}]}"#;
        assert!(parse_index(all_bad).is_err());
    }

    #[test]
    fn allows_gui_compares_versions() {
        let mut e = entry();
        assert!(e.allows_gui("0.3.0").is_ok());
        assert!(e.allows_gui("0.3.1").is_ok());
        assert!(e.allows_gui("1.0.0").is_ok());
        let err = e.allows_gui("0.2.1").unwrap_err().to_string();
        assert!(err.contains("需要 GUI"), "{err}");

        e.min_gui_version = String::new();
        assert!(e.allows_gui("0.0.1").is_ok());
    }

    #[test]
    fn verify_package_checks_hash_then_identity() {
        // 用真实 zip 太吵，这里只验证哈希这一关会拦住（其余分支由安装测试覆盖）
        let e = entry();
        let err = verify_package(&e, b"not a zip", "0.3.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("sha256 不匹配"), "{err}");
    }

    #[test]
    fn rows_mark_installed_and_update() {
        let e = entry();
        let installed = vec![Installed {
            id: "KVAULT".into(),
            version: "1.1.0".into(),
        }];
        let r = rows(&[e.clone()], &installed);
        assert_eq!(r.len(), 1);
        assert!(r[0].installed);
        assert!(r[0].update, "1.2.0 应比 1.1.0 新");
        assert_eq!(r[0].permissions.len(), 2, "两条权限应各自成项");

        let same = vec![Installed {
            id: "kvault".into(),
            version: "1.2.0".into(),
        }];
        assert!(!rows(&[e.clone()], &same)[0].update);

        let none: Vec<Installed> = Vec::new();
        let r = rows(&[e], &none);
        assert!(!r[0].installed);
        assert!(!r[0].update);
    }

    #[test]
    fn empty_capabilities_render_blank() {
        let mut e = entry();
        e.capabilities.clear();
        assert_eq!(e.permissions_text(), "");
        assert!(e.permissions_list().is_empty());
    }

    #[test]
    fn every_capability_group_has_a_label() {
        // 回归：`proc`/`net`/`rclone`/`env` 曾经落到"未识别的权限"分支，
        // 用户看到的是一堆看不懂的英文标识，等于没告诉他要授权什么。
        for g in plugin_host::config::CAP_GROUPS {
            let line = capability_line(g);
            assert!(
                !line.contains("未识别"),
                "能力组 {g} 应有自己的说明，实际是：{line}"
            );
            assert!(line.len() > 2, "能力组 {g} 的说明太短：{line}");
        }
        // 清单里写了不存在的组，必须照实标出来，不能编一句好听的盖过去。
        assert!(capability_line("systray").contains("未识别"));
    }

    /// 仓库里那份 `docs/store/index.json` 就是发给用户的清单：写错一个字符，
    /// 用户点安装会在最后一步被拒。这里直接读那份文件（编译期嵌进来，不联网、
    /// 不碰凭据），把「能不能装上」在测试里复现一遍。
    #[test]
    fn the_shipped_store_index_lets_kvault_in_only_from_0_3_1() {
        let parsed = parse_index(include_str!("../../../docs/store/index.json"))
            .expect("仓库里的商店索引读不出来");
        assert!(
            parsed.warnings.is_empty(),
            "索引里有条目过不了自身校验：{:?}",
            parsed.warnings
        );
        let kv = parsed
            .entries
            .iter()
            .find(|e| e.id == "kvault")
            .expect("商店索引里少了 kvault");
        assert_eq!(kv.version, "0.3.0", "版本要和发布出来的 zip 同名");
        assert_eq!(
            kv.url,
            "https://github.com/h897105167/rcgui-plugin-kvault/releases/download/v0.3.0/kvault-0.3.0.zip"
        );
        // 这条门槛是收录它的全部意义：0.3.1 之前的宿主没有插件远程协议，
        // 装进去只会在远程列表里显示成没人认领的类型。旧构建必须在安装前被拒。
        assert_eq!(kv.min_gui_version, "0.3.1");
        assert!(
            kv.allows_gui("0.3.0").is_err(),
            "0.3.0 的宿主不该装得上需要新协议的 K-Vault"
        );
        assert!(kv.allows_gui("0.3.1").is_ok());
        let mut caps = kv.capabilities.clone();
        caps.sort();
        assert_eq!(
            caps,
            ["http", "kv", "rclone", "ui"],
            "能力组要和包内 config.json 一致：多写一个就是多要一项授权"
        );
    }

    /// 随包发布的插件（`plugins/manifest.json`）如果也在商店里，两处必须指同一个
    /// 包。PowerShell 校验脚本只在 PR 流程里跑；这条让 `cargo test` 也能拦住
    /// 「改了一处忘了另一处」——那会让应用按清单拉的包和商店页面展示的不是一个东西。
    #[test]
    fn a_bundled_plugin_points_at_the_same_package_in_the_store() {
        let parsed = parse_index(include_str!("../../../docs/store/index.json"))
            .expect("仓库里的商店索引读不出来");
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../../../plugins/manifest.json"))
                .expect("plugins/manifest.json 不是合法 JSON");
        let list = manifest["plugins"]
            .as_array()
            .expect("manifest 缺少 plugins");
        for p in list {
            let id = p["id"].as_str().unwrap_or_default();
            let Some(store) = parsed.entries.iter().find(|e| e.id == id) else {
                continue; // 随包发布但没上架是允许的：商店只服务愿意自己装的用户
            };
            assert_eq!(
                p["sha256"].as_str().unwrap_or_default(),
                store.sha256,
                "{id} 在清单与商店里的 sha256 不一致：两个包不是同一个东西"
            );
            assert_eq!(
                p["tag"].as_str().unwrap_or_default(),
                format!("v{}", store.version),
                "{id} 的清单 tag 与商店 version 对不上"
            );
        }
    }

    #[test]
    fn parse_version_handles_prefix_and_prerelease() {
        assert_eq!(parse_version("v0.3.0"), Some((0, 3, 0)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("abc"), None);
    }
}
