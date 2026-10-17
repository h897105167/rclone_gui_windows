//! 端到端验证插件包安装流程（真实 zip 文件 → 校验 → 安装 → 扫描）。
//!
//! 用法: cargo run -p plugin_host --example pkg_probe -- <plugin.zip> <plugins-dir> [gui-version]

use plugin_host::package;
use std::path::Path;

fn main() {
    let mut args = std::env::args().skip(1);
    let zip_path = args.next().unwrap_or_else(|| {
        eprintln!("用法: pkg_probe <plugin.zip> <plugins-dir> [gui-version]");
        std::process::exit(2);
    });
    let plugins_dir = args.next().unwrap_or_else(|| {
        eprintln!("缺少 <plugins-dir>");
        std::process::exit(2);
    });
    let gui_version = args.next().unwrap_or_else(|| "0.1.0".to_string());

    let data = std::fs::read(&zip_path).expect("读取 zip 失败");
    println!("zip 大小: {} 字节", data.len());

    // 1) 大小检查
    match package::check_source_size(data.len() as u64) {
        Ok(()) => println!("[1] 大小检查通过"),
        Err(e) => {
            eprintln!("[1] 大小检查失败: {e}");
            std::process::exit(1);
        }
    }

    // 2) 元数据预检
    match package::peek_zip_meta(&data) {
        Ok(m) => println!(
            "[2] 元数据: id={} name={} version={} entry={} min_gui={}",
            m.id, m.name, m.version, m.entry_file, m.min_gui_version
        ),
        Err(e) => {
            eprintln!("[2] 元数据解析失败: {e:#}");
            std::process::exit(1);
        }
    }

    let root = Path::new(&plugins_dir);
    // 临时目录放在插件目录旁边：避免依赖 %TEMP% 的写入权限
    let tmp = root
        .parent()
        .map(|p| p.join("pkg-probe-tmp"))
        .unwrap_or_else(|| std::env::temp_dir().join("pkg-probe-tmp"));
    let _ = std::fs::create_dir_all(&tmp);
    println!("tmp: {}", tmp.display());

    // 3) 安装
    match package::install_from_zip(&data, root, &tmp, &gui_version) {
        Ok(o) => println!("[3] 安装结果: {}", o.message()),
        Err(e) => {
            eprintln!("[3] 安装失败: {e:#}");
            std::process::exit(1);
        }
    }

    // 4) 重新扫描（模拟 UI 刷新）
    let (ok, bad) = package::scan_dir(root, &gui_version);
    println!("[4] 扫描: 可用 {} 个, 损坏 {} 个", ok.len(), bad.len());
    for p in &ok {
        println!(
            "      OK  {} v{}  entry={}  dir={}",
            p.meta.id,
            p.meta.version,
            p.meta.entry_file,
            p.dir.display()
        );
    }
    for (d, e) in &bad {
        println!("      BAD {} : {}", d.display(), e);
    }

    // 5) 再装一次同版本 → 应保持不替换
    match package::install_from_zip(&data, root, &tmp, &gui_version) {
        Ok(o) => println!("[5] 重复安装: {}", o.message()),
        Err(e) => {
            eprintln!("[5] 重复安装失败: {e:#}");
            std::process::exit(1);
        }
    }

    if !bad.is_empty() {
        eprintln!("存在损坏插件，判定失败");
        std::process::exit(1);
    }
    println!("ALL OK");
}
