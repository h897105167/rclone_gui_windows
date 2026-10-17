//! rclone 进程管理（**legacy**）：`rclone.exe` 探测、托管 `rcd` 子进程、`cat` 流式读取。
//!
//! 阶段 2 已完成解耦：RC 客户端（HTTP + 鉴权 + 方法封装）与连接状态已迁往
//! `rcgui-rc`（crate `rcgui_rc`）。本 crate 现在**只剩进程生命周期**，
//! 且按重构指南的最终目标（RCGUI 不启停 rclone）将在阶段 7 整体移除。
//!
//! 为了不打断现有调用方，这里继续 re-export 已迁走的类型，使
//! `rclone_client::RcClient` 等旧路径在过渡期仍然可用。

mod proc;

pub use proc::{discover_exe, extract_windows_path, generate_password, RcloneManager};

// 过渡期兼容 re-export：调用方可继续 `use rclone_client::{...}`。
pub use rcgui_rc::{
    ConfigPaths, ConnStatus, CoreStats, Endpoint, ListEntry, MountInfo, RcClient, RcError,
    RcErrorKind, RcResult, RcState, Transferring, VersionInfo,
};

use serde_json::json;

/// 常用 options 集合：递归列出
pub fn list_opts_recurse() -> serde_json::Value {
    json!({ "recurse": true })
}

/// 本地路径转 rclone local backend 的 fs 字符串（去掉尾部分隔符）
pub fn local_fs(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().to_string();
    s.trim_end_matches(['\\', '/']).to_string()
}
