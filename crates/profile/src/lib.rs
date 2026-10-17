//! RCGUI 档案模型：**用户保存的期望配置**，与 rclone 的运行态严格分离。
//!
//! 为什么单独一个 crate：
//! - 阶段 5 的 `rcgui.conf` 导入导出、阶段 6 的恢复脚本都要读同一份模型；
//! - 模型必须能独立于 UI 与进程管理器做单元测试。
//!
//! 一条硬规则（重构指南 §3）：**`mount/listmounts` 返回的运行态不是可重建配置**。
//! 本 crate 只存用户当初填的参数；运行态由 `rcgui-rc` 的 DTO 表示，两者不互相推导。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod bundle;
pub mod restore;

pub use bundle::{
    diff as bundle_diff, BundleError, Diff, RcguiConf, RemoteConf, ScriptEntry, Summary,
    BUNDLE_SCHEMA_VERSION, KIND as BUNDLE_KIND,
};

/// 当前文档版本。任何字段增删都必须递增它，并在 `Store::load` 里写迁移分支。
pub const SCHEMA_VERSION: u32 = 1;

/// 档案元信息。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProfileMeta {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

/// 一个保存的连接（**绝不存密码明文**，只记录「有没有」）。
///
/// 首版只允许一个活动端点（指南 §3），但模型允许多条，便于阶段 5 的导入。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    /// 0 = 托管（自动启动 rcd，legacy）/ 1 = 连接已有实例
    pub mode: u8,
    /// attach 模式的完整地址
    pub url: String,
    /// 托管模式的 RC 端口
    pub port: u16,
    pub user: String,
    /// 是否配置了密码（明文只存在于 rclone/RCGUI 的凭据文件，不随档案流转）
    pub has_pass: bool,
}

/// 期望的挂载配置。字段与 UI 表单一一对应，**不是** `mount/listmounts` 的产物。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MountProfile {
    /// 远程名（不含结尾冒号）
    pub remote: String,
    /// 远程内子目录（空 = 根）
    pub sub_path: String,
    /// 目标盘符或挂载点，如 "T:"
    pub point: String,
    /// 只读
    pub ro: bool,
    /// VFS 缓存模式：off / minimal / writes / full
    pub cache: String,
    /// 是否启用（停用后启动时不自动挂载，配置保留）
    pub enabled: bool,
}

/// 期望的 serve 配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ServeProfile {
    /// webdav / sftp / http / ftp / dlna / nfs / docker
    pub stype: String,
    pub remote: String,
    pub sub_path: String,
    pub addr: String,
    pub user: String,
    /// 与连接密码同理：只记录是否存在，明文不落档案
    pub pass: String,
    pub enabled: bool,
}

/// serve 自定义名称（按 协议+远程+子路径+地址 匹配）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ServeNameProfile {
    pub stype: String,
    pub remote: String,
    pub sub_path: String,
    pub addr: String,
    pub name: String,
}

/// 远程的本地副本（**仅非敏感元信息**）。
///
/// 完整参数（可能含 OAuth token）只在用户显式导出 `rcgui.conf` 时按需写入，
/// 并附带敏感数据提示；平时不落盘，避免把凭据复制成第二份。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RemoteNote {
    pub name: String,
    pub rtype: String,
    /// 占用统计等展示用文本（可为空）
    pub usage: String,
    /// 是否缺少凭据（导入的是无凭据包，或凭据被剥离）→ UI 标为「待认证」
    pub needs_auth: bool,
}

/// 档案文档根。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProfileDoc {
    pub schema_version: u32,
    pub meta: ProfileMeta,
    pub connections: Vec<ConnectionProfile>,
    /// 活动连接 id（首版为空表示「用默认配置」）
    pub active_connection: Option<String>,
    pub mounts: Vec<MountProfile>,
    pub serves: Vec<ServeProfile>,
    pub serve_names: Vec<ServeNameProfile>,
    pub remotes: Vec<RemoteNote>,
}

impl Default for ProfileDoc {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            meta: ProfileMeta {
                id: "default".to_string(),
                name: "默认档案".to_string(),
                created_at: now_iso(),
                updated_at: now_iso(),
            },
            connections: Vec::new(),
            active_connection: None,
            mounts: Vec::new(),
            serves: Vec::new(),
            serve_names: Vec::new(),
            remotes: Vec::new(),
        }
    }
}

/// 「期望 vs 运行」的四态。UI 用它区分不同徽标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    /// 已保存但停用
    Saved,
    /// 已保存且正在运行
    Running,
    /// 已保存、已启用，但还没出现在运行态里（挂载中/启动中）
    Pending,
    /// 已保存，但当前没有连接，运行态未知
    Stale,
}

impl DesiredState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Saved => "已保存",
            Self::Running => "运行中",
            Self::Pending => "待生效",
            Self::Stale => "陈旧",
        }
    }
}

/// 计算某个预设的状态。纯函数，便于测试与在 UI 侧复用。
pub fn desired_state(enabled: bool, running: bool, pending: bool, connected: bool) -> DesiredState {
    if !connected {
        return DesiredState::Stale;
    }
    if running {
        return DesiredState::Running;
    }
    if enabled && pending {
        return DesiredState::Pending;
    }
    DesiredState::Saved
}

/// 盘符/挂载点归一化：`"T:"`、`"T:\\"`、`"t"` 视为同一处（历史 bug 来源）。
pub fn normalize_mount_point(p: &str) -> String {
    let t = p.trim().trim_end_matches(['\\', '/']);
    let t = t.trim_end_matches(':');
    if t.len() == 1 && t.chars().all(|c| c.is_ascii_alphabetic()) {
        format!("{}:", t.to_ascii_uppercase())
    } else {
        t.trim_end_matches(':').to_string()
    }
}

impl ProfileDoc {
    /// 远程名 → 是否「凭据未随包导出」。**精确匹配**；无对应档案记录返回 false（不显示徽标）。
    pub fn needs_auth_of(&self, name: &str) -> bool {
        self.remotes.iter().any(|r| r.name == name && r.needs_auth)
    }

    /// 需要显示徽标的远程名集合（供 UI 批量合并）。
    pub fn needs_auth_index(&self) -> std::collections::HashMap<String, bool> {
        self.remotes
            .iter()
            .filter(|r| r.needs_auth)
            .map(|r| (r.name.clone(), true))
            .collect()
    }
}

impl MountProfile {
    /// 运行态里是否已有这个挂载（按归一化盘符比较）。
    pub fn matches_running(&self, runtime_points: &[String]) -> bool {
        let want = normalize_mount_point(&self.point);
        runtime_points
            .iter()
            .any(|p| normalize_mount_point(p) == want)
    }
}

/// 档案读写错误。
#[derive(Debug, Clone)]
pub enum StoreError {
    Io(String),
    Parse(String),
    /// 文件版本比本程序新：拒绝降级写入，避免丢数据。
    VersionTooNew { found: u32, supported: u32 },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "档案读写失败: {e}"),
            Self::Parse(e) => write!(f, "档案格式无效: {e}"),
            Self::VersionTooNew { found, supported } => write!(
                f,
                "档案版本 {found} 高于本程序支持的 {supported}，请升级 RCGUI（已拒绝写入以免损坏数据）"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

/// 单文件档案存储（JSON，原子写）。
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取档案。`Ok(None)` 表示文件不存在（首次运行或纯 legacy 环境）。
    pub fn load(&self) -> Result<Option<ProfileDoc>, StoreError> {
        let raw = match std::fs::read(&self.path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::Io(e.to_string())),
        };
        let raw: &[u8] = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
            &raw[3..]
        } else {
            &raw
        };
        let mut doc: ProfileDoc =
            serde_json::from_slice(raw).map_err(|e| StoreError::Parse(e.to_string()))?;
        if doc.schema_version > SCHEMA_VERSION {
            return Err(StoreError::VersionTooNew {
                found: doc.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        // 低版本迁移：v0（无 schema_version 字段）→ v1 只需补默认值，字段层面兼容
        if doc.schema_version < SCHEMA_VERSION {
            doc.schema_version = SCHEMA_VERSION;
        }
        Ok(Some(doc))
    }

    /// 原子保存：先写临时文件再改名，避免半截文件。
    pub fn save(&self, doc: &ProfileDoc) -> Result<(), StoreError> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let mut d = doc.clone();
        d.schema_version = SCHEMA_VERSION;
        d.meta.updated_at = now_iso();
        if d.meta.id.is_empty() {
            d.meta.id = "default".to_string();
        }
        if d.meta.name.is_empty() {
            d.meta.name = "默认档案".to_string();
        }
        let data = serde_json::to_vec_pretty(&d).map_err(|e| StoreError::Parse(e.to_string()))?;
        let tmp = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, &data).map_err(|e| StoreError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            StoreError::Io(e.to_string())
        })?;
        Ok(())
    }
}


#[cfg(test)]
fn test_dir(tag: &str) -> std::path::PathBuf {
    // 本项目沙箱下 %TEMP% 不可写（os error 5），统一用测试可执行文件旁的目录
    let base = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(std::env::temp_dir);
    let d = base.join(format!("rcgui-profile-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
pub fn now_iso() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_doc_is_current_version() {
        let d = ProfileDoc::default();
        assert_eq!(d.schema_version, SCHEMA_VERSION);
        assert_eq!(d.meta.id, "default");
    }

    #[test]
    fn missing_fields_are_tolerated() {
        // 只有 schema_version 的极简文件也必须能解析（向前兼容老档案）。
        // 容器级 `#[serde(default)]` 会用 Default::default() 补齐缺失字段，
        // 因此 meta 拿到的是「默认档案」而不是空串。
        let doc: ProfileDoc = serde_json::from_str(r#"{"schema_version":1}"#).unwrap();
        assert!(doc.mounts.is_empty());
        assert_eq!(doc.schema_version, SCHEMA_VERSION);
        assert!(!doc.meta.name.is_empty(), "缺失字段应补齐为可用默认值");
    }

    #[test]
    fn mount_point_normalisation() {
        assert_eq!(normalize_mount_point("T:"), "T:");
        assert_eq!(normalize_mount_point("T:\\"), "T:");
        assert_eq!(normalize_mount_point(" t "), "T:");
        assert_eq!(normalize_mount_point("\\\\server\\share\\"), "\\\\server\\share");
    }

    #[test]
    fn matches_running_uses_normalised_key() {
        let m = MountProfile {
            point: "T:\\".into(),
            ..Default::default()
        };
        assert!(m.matches_running(&["T:".to_string()]));
        assert!(!m.matches_running(&["U:".to_string()]));
    }

    #[test]
    fn desired_state_matrix() {
        assert_eq!(desired_state(true, true, false, true), DesiredState::Running);
        assert_eq!(desired_state(true, false, true, true), DesiredState::Pending);
        assert_eq!(desired_state(true, false, false, true), DesiredState::Saved);
        assert_eq!(desired_state(false, false, false, true), DesiredState::Saved);
        assert_eq!(desired_state(true, false, true, false), DesiredState::Stale);
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = test_dir("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(dir.join("profiles").join("default.json"));
        let mut doc = ProfileDoc::default();
        doc.mounts.push(MountProfile {
            remote: "teldrive".into(),
            sub_path: "media".into(),
            point: "T:".into(),
            ro: true,
            cache: "writes".into(),
            enabled: true,
        });
        store.save(&doc).unwrap();
        let back = store.load().unwrap().expect("应能读回");
        assert_eq!(back.mounts.len(), 1);
        assert_eq!(back.mounts[0].point, "T:");
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_future_version() {
        let dir = test_dir("future");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("default.json");
        std::fs::write(&p, br#"{"schema_version": 99}"#).unwrap();
        let err = Store::new(&p).load().unwrap_err();
        assert!(matches!(err, StoreError::VersionTooNew { found: 99, .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_file_is_not_an_error() {
        let p = std::env::temp_dir().join("rcgui-profile-does-not-exist.json");
        let _ = std::fs::remove_file(&p);
        assert!(Store::new(&p).load().unwrap().is_none());
    }
}

#[cfg(test)]
mod badge_tests {
    use super::*;

    fn doc_with(needs_auth: bool) -> ProfileDoc {
        let mut d = ProfileDoc::default();
        d.remotes.push(RemoteNote {
            name: "teldrive".into(),
            rtype: "teldrive".into(),
            usage: String::new(),
            needs_auth,
        });
        d
    }

    #[test]
    fn badge_shows_when_credentials_were_stripped() {
        // 导入的是「不含凭据」的档案 → needs_auth=true → 应显示徽标
        let d = doc_with(true);
        assert!(d.needs_auth_of("teldrive"));
        assert_eq!(d.needs_auth_index().get("teldrive"), Some(&true));
    }

    #[test]
    fn badge_hidden_when_credentials_present() {
        // 导入的是「含凭据」的档案 → needs_auth=false → 不显示徽标
        let d = doc_with(false);
        assert!(!d.needs_auth_of("teldrive"));
        assert!(d.needs_auth_index().is_empty());
    }

    #[test]
    fn badge_hidden_without_archive_record() {
        // 档案里没有该远程（例如刚新建、尚未导出过）→ 不显示徽标
        let d = doc_with(true);
        assert!(!d.needs_auth_of("other"));
        assert!(!d.needs_auth_of("TELDRIVE"), "按名称精确匹配，大小写不同即视为无记录");
    }
}