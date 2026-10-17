//! 用真实 zip 文件验证读取器（Windows Compress-Archive 等工具可能使用
//! data descriptor / 不同压缩参数，需要确认能正确解析）。
//!
//! 用法: cargo run -p plugin_host --example zip_probe -- <path-to-zip>

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("用法: zip_probe <path-to-zip>");
            std::process::exit(2);
        }
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("读取失败 {path}: {e}");
            std::process::exit(1);
        }
    };
    println!("文件大小: {} 字节", data.len());

    let entries = match plugin_host::zip::read_entries(&data) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("解析中央目录失败: {e:#}");
            std::process::exit(1);
        }
    };
    println!("条目数: {}", entries.len());
    for e in &entries {
        println!(
            "  {}  method={} comp={} raw={} dir={}",
            e.name, e.method, e.compressed_size, e.uncompressed_size, e.is_dir
        );
        match plugin_host::zip::read_entry(&data, e) {
            Ok(c) => println!("    -> 解出 {} 字节 OK", c.len()),
            Err(err) => {
                eprintln!("    -> 解压失败: {err:#}");
                std::process::exit(1);
            }
        }
    }

    match plugin_host::zip::find_plugin_json(&data) {
        Ok(Some((n, c))) => {
            println!("找到 plugin.json（{n}）:");
            println!("{}", String::from_utf8_lossy(&c));
            match plugin_host::package::peek_zip_meta(&data) {
                Ok(m) => println!("元数据解析 OK: id={} version={}", m.id, m.version),
                Err(e) => {
                    eprintln!("元数据解析失败: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        Ok(None) => println!("未找到 plugin.json"),
        Err(e) => {
            eprintln!("查找 plugin.json 失败: {e:#}");
            std::process::exit(1);
        }
    }
    println!("ALL OK");
}
