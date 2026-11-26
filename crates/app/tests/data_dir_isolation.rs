//! 证明 `RCGUI_DATA_DIR` 真的把一个**独立进程**搬到了临时目录。
//!
//! 单元测试只能证明解析函数算得对；这里启动真正的 `rclone-gui.exe`，用它的诊断输出
//! 回读数据目录。父会话做「真实 rclone 本地后端」验收时要靠这条隔离：不隔离就会读写
//! 用户 `%APPDATA%\RCGUI` 里的真配置，一次验收便可能改掉用户的生产设置。
//!
//! 这里刻意不检查退出码以外的东西有没有连网、开窗：`--diagnostics` 的设计前提就是
//! 「不启动内核、不开窗口」，测试里断言它仍然满足该前提。

use std::path::PathBuf;
use std::process::Command;

fn exe() -> &'static str {
    env!("CARGO_BIN_EXE_rclone-gui")
}

/// 每个用例一个独立目录，避免并行测试互相踩。
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rcgui-iso-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn diagnostics_reports_the_overridden_data_dir() {
    let dir = scratch("diag");
    let out = Command::new(exe())
        .env("RCGUI_DATA_DIR", &dir)
        .args(["--diagnostics", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "诊断应当成功退出：{:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let text = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("诊断输出不是合法 JSON（{e}）：\n{text}"));

    // 输出里的路径会把用户主目录换成 %USERPROFILE%，所以只比对最后一段。
    let reported = json["data_dir"].as_str().unwrap_or_default();
    let leaf = dir.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        reported.ends_with(&leaf),
        "诊断里的 data_dir 应当是覆盖后的目录（…{leaf}），实际是 {reported}"
    );
    assert_eq!(
        json["version"].as_str().unwrap_or_default(),
        env!("CARGO_PKG_VERSION"),
        "版本号来自同一个 cargo 包，应当一致"
    );
    // 覆盖目录里会有一个默认档案（首次运行本来就该有），但它必须是**空的**：
    // 从用户 `%APPDATA%\RCGUI` 复制过去的挂载、远程、代理会让验收脚本在核对期间
    // 操作用户真实的后端，这是隔离最需要防的一件事。
    let profile = dir.join("profiles").join("default.json");
    let raw = std::fs::read_to_string(&profile)
        .unwrap_or_else(|e| panic!("首次运行应当写出默认档案 {}：{e}", profile.display()));
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    for key in ["mounts", "serves", "remotes", "proxies", "remote_proxies"] {
        let list = doc[key]
            .as_array()
            .unwrap_or_else(|| panic!("档案里没有 {key} 数组：{raw}"));
        assert!(
            list.is_empty(),
            "隔离目录的档案里不应该是用户的东西（{key}）：{raw}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// 两个不同的覆盖目录必须是两个独立的实例：单实例锁按目录作用域，不能把第二个
/// 验收目录的进程重定向到第一个实例上（那会让父会话连上别人的窗口）。
#[test]
fn different_data_dirs_do_not_share_one_instance() {
    let a = scratch("inst-a");
    let b = scratch("inst-b");

    let first = Command::new(exe())
        .env("RCGUI_DATA_DIR", &a)
        .args(["--diagnostics", "--json"])
        .output()
        .unwrap();
    let second = Command::new(exe())
        .env("RCGUI_DATA_DIR", &b)
        .args(["--diagnostics", "--json"])
        .output()
        .unwrap();

    assert!(first.status.success());
    assert!(second.status.success());
    let leaf_a = a.file_name().unwrap().to_string_lossy().to_string();
    let leaf_b = b.file_name().unwrap().to_string_lossy().to_string();
    let ja: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let jb: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert!(ja["data_dir"].as_str().unwrap_or("").ends_with(&leaf_a));
    assert!(jb["data_dir"].as_str().unwrap_or("").ends_with(&leaf_b));

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

/// 不认识的参数必须明确报错退出，而不是「没看懂就当打开窗口」。
/// 用 `--bad` 是常态：脚本拼错一个开关，用户最不希望看到的是弹出一个 GUI。
#[test]
fn unknown_flag_exits_non_zero_without_starting_the_gui() {
    let dir = scratch("badflag");
    let out = Command::new(exe())
        .env("RCGUI_DATA_DIR", &dir)
        .arg("--bad-flag-nobody-typed")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "参数错误应当用退出码 2");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--bad-flag-nobody-typed"),
        "错误信息要点名那个不认识的参数：{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
