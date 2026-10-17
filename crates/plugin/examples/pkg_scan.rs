//! 只扫描插件目录（不安装），用于验证完整性检测。
//!
//! 用法: cargo run -p plugin_host --example pkg_scan -- <plugins-dir> [gui-version] [--run <plugin-id>]

use plugin_host::package;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args.first().cloned().unwrap_or_else(|| {
        eprintln!("用法: pkg_scan <plugins-dir> [gui-version] [--run <plugin-id>]");
        std::process::exit(2);
    });
    let gui_version = args
        .get(1)
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "0.1.0".to_string());

    let (ok, bad) = package::scan_dir(std::path::Path::new(&dir), &gui_version);
    println!("可用: {}", ok.len());
    for p in &ok {
        println!("  OK  {} v{}  entry={}", p.meta.id, p.meta.version, p.meta.entry_file);
    }
    println!("损坏: {}", bad.len());
    for (d, e) in &bad {
        println!("  BAD {} : {}", d.display(), e);
    }

    // --run <id>：执行该插件的 entry_file（等价于 UI 里勾选启用）
    if let Some(i) = args.iter().position(|a| a == "--run") {
        let want = args.get(i + 1).cloned().unwrap_or_default();
        let Some(p) = ok.iter().find(|p| p.meta.id == want) else {
            eprintln!("找不到插件 {want}");
            std::process::exit(1);
        };
        println!("执行入口文件: {}", p.meta.entry_file);
        match package::run_entry_file(&p.dir, &p.meta) {
            Ok(0) => println!("入口文件执行成功（退出码 0）"),
            Ok(c) => {
                eprintln!("入口文件退出码 {c}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("执行失败: {e:#}");
                std::process::exit(1);
            }
        }
    }

    if !bad.is_empty() {
        std::process::exit(1);
    }
}
