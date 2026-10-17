//! RC API 数据类型（字段名兼容 rclone RC 返回结构）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VersionInfo {
    #[serde(rename = "version", default)]
    pub version: String,
    #[serde(rename = "os", default)]
    pub os: String,
    #[serde(rename = "arch", default)]
    pub arch: String,
    #[serde(rename = "goVersion", default)]
    pub go_version: String,
    #[serde(rename = "isGit", default)]
    pub is_git: bool,
    /// rclone 返回的版本分解，如 `[1, 75, 2]`；用于版本门槛判定。
    #[serde(rename = "decomposed", default)]
    pub decomposed: Vec<u32>,
}

impl VersionInfo {
    /// 是否至少为 `major.minor`。`decomposed` 缺失时退化为解析 `version` 字符串。
    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        let (maj, min) = match (self.decomposed.first(), self.decomposed.get(1)) {
            (Some(m), Some(n)) => (*m, *n),
            _ => {
                let mut it = self.version.trim_start_matches('v').split('.');
                let m = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let n = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                (m, n)
            }
        };
        (maj, min) >= (major, minor)
    }
}

/// `config/paths` 的返回：rclone 实际使用的目录。
///
/// 用途：握手时向**运行的实例**询问真实配置路径，而不是假设
/// `%APPDATA%\rclone\rclone.conf`，也不需要启动 `rclone config file` 子进程。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigPaths {
    #[serde(default)]
    pub config: String,
    #[serde(default)]
    pub cache: String,
    #[serde(default)]
    pub temp: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListEntry {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "Path", default)]
    pub path: String,
    #[serde(rename = "Size", default)]
    pub size: i64,
    #[serde(rename = "ModTime", default)]
    pub mod_time: String,
    #[serde(rename = "IsDir", default)]
    pub is_dir: bool,
    #[serde(rename = "MimeType", default)]
    pub mime_type: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Transferring {
    #[serde(rename = "name", default)]
    pub name: String,
    #[serde(rename = "size", default)]
    pub size: f64,
    #[serde(rename = "bytes", default)]
    pub bytes: f64,
    #[serde(rename = "speed", default)]
    pub speed: f64,
    #[serde(rename = "eta", default)]
    pub eta: f64,
    #[serde(rename = "percentage", default)]
    pub percentage: f64,
    #[serde(rename = "group", default)]
    pub group: String,
    /// 源文件系统（如 "teldrive:path" 或本地 "//?/C:/..."）；用于推导远程名与传输方向
    #[serde(rename = "srcFs", default)]
    pub src_fs: String,
    /// 目标文件系统
    #[serde(rename = "dstFs", default)]
    pub dst_fs: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoreStats {
    #[serde(rename = "speed", default)]
    pub speed: f64,
    #[serde(rename = "bytes", default)]
    pub bytes: f64,
    #[serde(rename = "totalBytes", default)]
    pub total_bytes: f64,
    #[serde(rename = "errors", default)]
    pub errors: i64,
    #[serde(rename = "transferring", default)]
    pub transferring: Vec<Transferring>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MountInfo {
    #[serde(default)]
    pub mount_point: String,
    #[serde(default)]
    pub fs: String,
}

/// 宽容解析 mount/list 返回（不同版本结构有差异）
pub fn parse_mounts(v: serde_json::Value) -> Vec<MountInfo> {
    fn get_str(o: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> String {
        for k in keys {
            if let Some(s) = o.get(*k).and_then(|x| x.as_str()) {
                return s.to_string();
            }
        }
        String::new()
    }
    let mut out = Vec::new();
    let arr = match v {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(o) => {
            if let Some(serde_json::Value::Array(a)) = o.get("mountPoints") {
                a.clone()
            } else if let Some(serde_json::Value::Array(a)) = o.get("MountPoints") {
                a.clone()
            } else {
                // 退化为 map：mountPoint -> 详情
                o.into_values().collect()
            }
        }
        _ => Vec::new(),
    };
    for it in arr {
        if let serde_json::Value::Object(o) = it {
            let mp = get_str(&o, &["MountPoint", "mountPoint", "mount_point"]);
            let fs = get_str(&o, &["Fs", "fs"]);
            if !mp.is_empty() {
                out.push(MountInfo { mount_point: mp, fs });
            }
        }
    }
    out
}

/// 字节数格式化
pub fn fmt_bytes(b: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = b;
    if v < 1024.0 {
        return format!("{v:.0} B");
    }
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

/// 秒数格式化为 mm:ss / hh:mm:ss
pub fn fmt_eta(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return "-".into();
    }
    let s = secs as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}
