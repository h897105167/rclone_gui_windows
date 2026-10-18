use std::path::{Path, PathBuf};

fn main() {
    // 1) Slint UI 编译
    let ui = PathBuf::from("ui/main.slint");
    if ui.exists() {
        slint_build::compile(&ui).expect("Slint UI 编译失败");
    }

    // 2) 嵌入 imagehost wasm（存在时）：优先与当前 profile 相同，其次另一个
    //
    // 注意：**不要**删掉这一段。`crates/app/src/main.rs` 用
    // `include_bytes!(env!("IMAGEHOST_WASM"))` 把内置插件编进程序，
    // 构建顺序必须是「先 WASM 插件、再应用」（见 scripts/build.ps1）。
    //
    // 已移除的部分（阶段 7）：内嵌 rclone.exe。RCGUI 不拥有 rclone 生命周期，
    // 不再需要把 82 MB 的 rclone 编进可执行文件。
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let mut wasm_dir = None;
    for base in ["..", "../.."] {
        for prof in [profile.as_str(), if profile == "release" { "debug" } else { "release" }] {
            let p = PathBuf::from(base)
                .join("target")
                .join("wasm32-wasip1")
                .join(prof)
                .join("imagehost.wasm");
            if p.exists() {
                wasm_dir = Some(p);
                break;
            }
        }
        if wasm_dir.is_some() {
            break;
        }
    }
    match wasm_dir {
        Some(p) => {
            let abs = std::fs::canonicalize(&p).unwrap_or(p);
            println!("cargo:rustc-env=IMAGEHOST_WASM={}", abs.display());
            println!("cargo:rerun-if-changed={}", abs.display());
        }
        None => {
            // 空路径会让 main.rs 的 include_bytes! 编译失败，因此这里给一个明确的构建期错误，
            // 而不是留下一个只在运行时才发现的「内置插件缺失」。
            panic!(
                "imagehost.wasm 未找到：请先 cargo build -p imagehost-plugin --target wasm32-wasip1"
            );
        }
    }

    // 3) 把内置插件资产复制到 exe 同级目录
    //
    // `crates/app/src/plugins.rs::builtin_root()` 在运行期读 `<exe>/assets/plugins`，
    // 而 cargo 不会把非 Rust 资产复制到 target/<profile>/。这里在构建期补上，
    // 好处是 `cargo build -p rclone-gui` 一条命令就能得到一个可直接运行的目录。
    //
    // 资产由 scripts/build.ps1 生成（编 wasm → 复制 config.json/main.wasm → 生成 check.sha256）。
    println!("cargo:rerun-if-changed=assets/plugins");
    let assets = PathBuf::from("assets/plugins");
    if assets.is_dir() {
        match target_profile_dir() {
            Some(dst) => {
                let dst = dst.join("assets").join("plugins");
                if let Err(e) = copy_dir(&assets, &dst) {
                    println!("cargo:warning=复制内置插件资产失败: {e}");
                }
            }
            None => println!("cargo:warning=无法定位 target profile 目录，内置插件资产未复制"),
        }
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
        } else {
            let same_len = std::fs::metadata(&to)
                .ok()
                .zip(std::fs::metadata(&from).ok())
                .map(|(a, b)| a.len() == b.len())
                .unwrap_or(false);
            if !same_len {
                std::fs::copy(&from, &to)?;
            }
        }
    }
    Ok(())
}
