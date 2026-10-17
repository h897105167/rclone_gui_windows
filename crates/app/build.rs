use std::path::PathBuf;

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
}
