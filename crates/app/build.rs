use std::path::{Path, PathBuf};

fn main() {
    // 1) Slint UI 编译
    //
    // 多语言：原始语言就是中文（`@tr("中文")` 里的中文即 msgid），`ui/i18n/<lang>/
    // LC_MESSAGES/<domain>.po` 里放其它语言的翻译，域名默认取 `CARGO_PKG_NAME`
    // （即 `rclone-gui`）。运行期用 `slint::select_bundled_translation("en")`
    // 切到英文、传空串切回中文。
    //
    // `with_default_translation_context(None)` 关掉「默认上下文 = 组件名」：
    // 否则同一个中文串在不同组件里会生成不同 msgid，翻译文件得写多份，漏一份
    // 就回落成中文。统一不带上下文，一个串只对应一条翻译。
    let ui = PathBuf::from("ui/main.slint");
    if ui.exists() {
        let config = slint_build::CompilerConfiguration::new()
            .with_bundled_translations("ui/i18n")
            .with_default_translation_context(slint_build::DefaultTranslationContext::None);
        println!("cargo:rerun-if-changed=ui/i18n");
        slint_build::compile_with_config(&ui, config).expect("Slint UI 编译失败");
    }

    // 2) 插件**不再**编进 exe
    //
    // 插件与本体彻底分离：每个插件都有自己的仓库与 Release，本体仓库只保存
    // `plugins/manifest.json`（id / repo / tag / sha256 的锁文件）。
    // 构建前用 `scripts/fetch_plugins.ps1` 按锁文件下载、校验、解包到
    // `crates/app/assets/plugins/<dir>/`（config.json + 入口 wasm + check.sha256）。
    //
    // 因此这里不再有 `cargo:rustc-env=IMAGEHOST_WASM`，也不再要求
    // 「先编 WASM 插件、再编 app」的顺序 —— 那个顺序是 `include_bytes!` 时代
    // 的历史包袱（详见 docs/PLUGIN.md「为什么要改成 Release 分发」）。
    println!("cargo:rerun-if-changed=../../plugins/manifest.json");

    // 3) 把插件资产复制到 exe 同级目录
    //
    // `crates/app/src/plugins.rs::builtin_root()` 在运行期读 `<exe>/assets/plugins`，
    // 而 cargo 不会把非 Rust 资产复制到 target/<profile>/。这里在构建期补上，
    // 好处是 `cargo build -p rclone-gui` 一条命令就能得到一个可直接运行的目录。
    //
    // 资产由 `scripts/fetch_plugins.ps1` 从各插件仓库的 Release 下载并校验后生成。
    println!("cargo:rerun-if-changed=assets/plugins");
    let assets = PathBuf::from("assets/plugins");
    if assets.is_dir() {
        match target_profile_dir() {
            Some(dst) => {
                let dst = dst.join("assets").join("plugins");
                if let Err(e) = copy_dir(&assets, &dst) {
                    println!("cargo:warning=复制插件资产失败: {e}");
                }
            }
            None => println!("cargo:warning=无法定位 target profile 目录，插件资产未复制"),
        }
    } else {
        println!(
            "cargo:warning=assets/plugins 不存在：请先运行 scripts/fetch_plugins.ps1 拉取插件资产"
        );
    }
}

/// 从 `OUT_DIR`（`<target>/<profile>/build/<pkg>-<hash>/out`）上溯三级得到 `<target>/<profile>`。
fn target_profile_dir() -> Option<PathBuf> {
    let out = PathBuf::from(std::env::var_os("OUT_DIR")?);
    let up = |p: &Path, n: usize| -> Option<PathBuf> {
        let mut cur = p.to_path_buf();
        for _ in 0..n {
            cur = cur.parent()?.to_path_buf();
        }
        Some(cur)
    };
    let dir = up(&out, 3)?;
    // 只接受确实以 profile 名结尾的路径，避免把资产复制到意料之外的位置
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    if dir.file_name().map(|n| n == profile.as_str()).unwrap_or(false) {
        Some(dir)
    } else {
        None
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else if needs_copy(&from, &to) {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 目标文件是否需要覆盖。
///
/// 只看长度是**不够的**：`check.sha256` 的内容是「哈希 + 两个空格 + 文件名」，
/// 重新打包后长度几乎必然还是同一个值（本项目的清单固定 154 字节）。
/// 只比长度会让清单永远停在旧版本，而 `main.wasm` / `config.json` 已经更新 ——
/// 装到用户目录后哈希对不上，插件显示「校验失败」且无法启用。
/// 因此同时比较修改时间：任一不同就复制。
fn needs_copy(from: &Path, to: &Path) -> bool {
    let (Ok(a), Ok(b)) = (std::fs::metadata(from), std::fs::metadata(to)) else {
        return true;
    };
    if a.len() != b.len() {
        return true;
    }
    let mtime = |m: &std::fs::Metadata| m.modified().ok();
    match (mtime(&a), mtime(&b)) {
        (Some(x), Some(y)) => x != y,
        // 拿不到时间戳时宁可多复制一次，也不要留下旧文件
        _ => true,
    }
}
