fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let ui = std::path::Path::new(&manifest).join("ui/main.slint");
    if ui.exists() {
        slint_build::compile(&ui).expect("Slint 编译失败");
    }
}
