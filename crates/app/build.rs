use std::path::PathBuf;

fn main() {
    // Slint UI 编译
    let ui = PathBuf::from("ui/main.slint");
    if ui.exists() {
        slint_build::compile(&ui).expect("Slint UI 编译失败");
    }

    // 嵌入 imagehost wasm（存在时）：优先 release，其次 debug
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let mut wasm_dir = None;
    for base in ["..", "../.."] {
        let p = PathBuf::from(base)
            .join("target")
            .join("wasm32-wasip1")
            .join(&profile)
            .join("imagehost.wasm");
        if p.exists() {
            wasm_dir = Some(p);
            break;
        }
        // profile 不匹配时用另一个
        let other = if profile == "release" { "debug" } else { "release" };
        let p2 = PathBuf::from(base)
            .join("target")
            .join("wasm32-wasip1")
            .join(other)
            .join("imagehost.wasm");
        if p2.exists() {
            wasm_dir = Some(p2);
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
            println!("cargo:rustc-env=IMAGEHOST_WASM=");
            println!("cargo:warning=imagehost.wasm 未找到：请先 cargo build -p imagehost-plugin --target wasm32-wasip1");
        }
    }
}
