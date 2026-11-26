use std::path::{Path, PathBuf};

// 图标画法在 runtime 和 build script 之间共用同一份源码：两边各自读，
// 就不会出现「exe 里的图标和托盘图标不是一张图」这种偏差。
#[path = "src/icon_asset.rs"]
mod icon_asset;

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

    // 4) exe 图标资源
    //
    // 任务栏/资源管理器/快捷方式上的图标来自 exe 自己的图标资源，不是运行期画的。
    // 没有这一步，exe 就没有图标资源，任务栏只能显示默认白纸图标（托盘却因为
    // 运行期用 RGBA 现画而正常），正好是用户报的那个「托盘有、任务栏没有」。
    embed_icon_resource();
}

/// 生成 `assets/rcgui.ico` 并把它编成 `.res` 链进 exe。
///
/// 失败只告警不中断：图标缺失不该让整个构建失败，测试环境也可能没有 rc.exe。
fn embed_icon_resource() {
    println!("cargo:rerun-if-changed=src/icon_asset.rs");

    // 交叉编译到非 Windows 目标时没有资源这一说
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        println!("cargo:warning=非 msvc 目标，跳过 exe 图标资源");
        return;
    }

    // 只在内容变了才重写：不然每次构建都改文件时间，触发无谓的全量重编。
    let ico = PathBuf::from("assets/rcgui.ico");
    let bytes = icon_asset::ico_file(icon_asset::APP_ICON_SIZES, icon_asset::ICON_ACCENT);
    let changed = std::fs::read(&ico).map(|old| old != bytes).unwrap_or(true);
    if changed {
        if let Some(dir) = ico.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&ico, &bytes) {
            println!("cargo:warning=写 assets/rcgui.ico 失败: {e}");
            return;
        }
    }

    let Some(rc_exe) = find_rc_exe() else {
        println!("cargo:warning=找不到 rc.exe（Windows SDK 资源编译器），exe 将没有图标资源");
        return;
    };
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let rc_file = out_dir.join("rcgui.rc");
    // .rc 里的路径用反斜杠、双写引号；这里只引用一个绝对路径，不依赖任何 include
    let ico_abs = std::fs::canonicalize(&ico).unwrap_or_else(|_| ico.clone());
    let rc_src = format!(
        "1 ICON \"{}\"\n",
        ico_abs.display().to_string().replace('\\', "\\\\")
    );
    if std::fs::write(&rc_file, rc_src).is_err() {
        println!("cargo:warning=写 rcgui.rc 失败，exe 将没有图标资源");
        return;
    }
    let res_file = out_dir.join("rcgui.res");
    let out = std::process::Command::new(&rc_exe)
        .arg("/nologo")
        .arg(format!("/fo{}", res_file.display()))
        .arg(&rc_file)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            println!("cargo:rustc-link-arg={}", res_file.display());
        }
        Ok(o) => println!(
            "cargo:warning=rc.exe 失败（{:?}）：{}",
            o.status.code(),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => println!("cargo:warning=跑 rc.exe 失败: {e}"),
    }
}

/// 定位 Windows SDK 的 `rc.exe`：先认 `RC` 环境变量，再找 PATH，最后扫 SDK 目录。
///
/// 扫 SDK 时取版本号最大的那个：机器上常同时装了多个 SDK，新的那个才有 x64 目录。
fn find_rc_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RC") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("rc.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    let roots = [
        std::env::var_os("ProgramFiles(x86)").map(PathBuf::from),
        std::env::var_os("ProgramFiles").map(PathBuf::from),
    ];
    let mut best: Option<(Vec<u32>, PathBuf)> = None;
    for root in roots.into_iter().flatten() {
        let bin = root.join("Windows Kits").join("10").join("bin");
        let Ok(entries) = std::fs::read_dir(&bin) else {
            continue;
        };
        for e in entries.flatten() {
            let cand = e.path().join("x64").join("rc.exe");
            if !cand.is_file() {
                continue;
            }
            let ver: Vec<u32> = e
                .file_name()
                .to_string_lossy()
                .split('.')
                .filter_map(|s| s.parse().ok())
                .collect();
            if best.as_ref().map(|(v, _)| &ver > v).unwrap_or(true) {
                best = Some((ver, cand));
            }
        }
    }
    best.map(|(_, p)| p)
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
    if dir
        .file_name()
        .map(|n| n == profile.as_str())
        .unwrap_or(false)
    {
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
