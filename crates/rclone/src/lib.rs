//! rclone 集成层：进程管理（rcd 托管/连接已有实例）+ RC API 客户端。

mod proc;
mod rc;
mod types;

pub use proc::{discover_exe, extract_windows_path, generate_password, RcloneManager};
pub use rc::RcClient;
pub use types::{CoreStats, ListEntry, MountInfo, Transferring, VersionInfo};

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
