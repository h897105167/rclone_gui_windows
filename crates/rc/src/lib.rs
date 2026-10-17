//! rcgui-rc：rclone RC 客户端 + 连接状态。
//!
//! 拆分背景（重构指南 §7.2）：原 `rclone_client` crate 把「RC HTTP 调用」和
//! 「启动/终止 rclone 子进程」焊在一起，导致 UI 层想调一个 API 就必须持有进程管理器。
//! 本 crate 只负责：
//!
//! - `RcClient`：端点地址 + 鉴权 + 方法封装（零 `std::process`）；
//! - `RcState`：当前连到哪个端点、是否连上、失败原因；
//! - `RcError`：错误分类，供 UI 分别报告连接/鉴权/不支持/超时。
//!
//! 进程生命周期仍留在 `rclone_client::proc`（legacy），阶段 7 移除。

pub mod client;
pub mod conn;
pub mod error;
pub mod fetch;
pub mod handshake;
pub mod types;

pub use client::{parse_methods, RcClient};
pub use conn::{ConnStatus, Endpoint, RcState};
pub use error::{RcError, RcErrorKind, RcResult};
pub use fetch::{fetch_to_temp, unique_tmp_name, TempFile};
pub use handshake::{Handshake, MethodReq, METHOD_REQS, MIN_MAJOR, MIN_MINOR};
pub use types::{
    fmt_bytes, fmt_eta, parse_mounts, ConfigPaths, CoreStats, ListEntry, MountInfo, Transferring,
    VersionInfo,
};
