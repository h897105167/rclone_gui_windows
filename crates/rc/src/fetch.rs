//! 通过 RC 把远端文件取到本地临时文件，再读成字节。
//!
//! 背景（实测）：`rclone v1.75.2-extra` **没有** `operations/cat`（调用返回
//! 404 `couldn't find method`）；可用替代是 `operations/copyfile` 复制到本地
//! 临时目录后读取，二进制逐字节一致，且整个路径只走 RC、不启动任何 rclone 子进程。
//!
//! 并发与清理：
//! - 每次调用生成**唯一**临时文件名（pid + 进程内单调序号 + 纳秒），并发请求不会互相覆盖；
//! - `TempFile` 的 `Drop` 负责删除，复制前就建立守卫，因此「复制中途失败留下的半截文件」
//!   也会被清理。

use crate::client::RcClient;
use crate::error::{RcError, RcErrorKind, RcResult};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 唯一临时文件名。
pub fn unique_tmp_name() -> String {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("rcgui-fetch-{}-{}-{}.bin", std::process::id(), seq, nanos)
}

/// 临时文件守卫：离开作用域即删除，异常路径同样生效。
pub struct TempFile {
    path: PathBuf,
}

impl TempFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 本地路径 → rclone local backend 的 fs 字符串。
fn local_fs(p: &Path) -> String {
    p.to_string_lossy().trim_end_matches(['\\', '/']).to_string()
}

/// 取远端文件字节。失败时不会留下临时文件。
pub async fn fetch_to_temp(
    client: &RcClient,
    src_fs: &str,
    src_remote: &str,
    tmp_dir: &Path,
) -> RcResult<Vec<u8>> {
    std::fs::create_dir_all(tmp_dir).map_err(|e| {
        RcError::new(
            RcErrorKind::Api,
            "fetch_to_temp",
            format!("创建临时目录 {} 失败: {e}", tmp_dir.display()),
        )
    })?;
    // 先建守卫再复制：复制中途失败可能留下半截文件
    let guard = TempFile {
        path: tmp_dir.join(unique_tmp_name()),
    };
    let name = guard
        .path()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    client
        .copyfile(src_fs, src_remote, &local_fs(tmp_dir), &name)
        .await?;
    std::fs::read(guard.path()).map_err(|e| {
        RcError::new(
            RcErrorKind::Api,
            "fetch_to_temp",
            format!("读取临时文件失败: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// 极简假 RC：解析 /operations/copyfile，按 dstFs/dstRemote 落盘，然后回 200。
    /// `fail` 为 true 时回 500 + {"error": "backend boom"}（用于验证错误透传）。
    fn spawn_fake_rc(payload: Vec<u8>, fail: bool) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // 读到 header 结束
                loop {
                    match sock.read(&mut chunk) {
                        Ok(0) => return,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => return,
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let clen = text
                    .lines()
                    .find_map(|l| {
                        let l = l.to_ascii_lowercase();
                        l.strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let header_end = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|p| p + 4)
                    .unwrap_or(buf.len());
                let mut body = buf[header_end..].to_vec();
                while body.len() < clen {
                    match sock.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                }

                if fail {
                    let resp = "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 25\r\nConnection: close\r\n\r\n{\"error\":\"backend boom\"}  ";
                    let _ = sock.write_all(resp.as_bytes());
                    return;
                }
                // 解析 JSON，按 dstFs/dstRemote 写文件（模拟 rclone 的真正行为）
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
                    let dst_fs = v.get("dstFs").and_then(|x| x.as_str()).unwrap_or("");
                    let dst = v.get("dstRemote").and_then(|x| x.as_str()).unwrap_or("");
                    if !dst_fs.is_empty() && !dst.is_empty() {
                        let dir = std::path::Path::new(dst_fs);
                        let _ = std::fs::create_dir_all(dir);
                        let _ = std::fs::write(dir.join(dst), &payload);
                    }
                }
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn tmp(tag: &str) -> PathBuf {
        // 注意：本项目沙箱下 %TEMP% 不可写（os error 5），测试目录放在测试可执行文件旁
        let base = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!("rcgui-fetch-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn count_files(dir: &Path) -> usize {
        std::fs::read_dir(dir).map(|r| r.count()).unwrap_or(0)
    }

    #[tokio::test]
    async fn binary_bytes_are_bit_exact_including_nul() {
        // 含 0x00 / 0xFF / 0xFE 的 14 字节 PNG 头样本
        let payload: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0xFF, 0xFE, 0x41, 0x42, 0x00,
        ];
        let (url, h) = spawn_fake_rc(payload.clone(), false);
        let dir = tmp("ok");
        let client = RcClient::new(url, None, None);
        let got = fetch_to_temp(&client, "teldrive:media", "a.png", &dir)
            .await
            .expect("应取回字节");
        assert_eq!(got, payload, "二进制必须逐字节一致");
        assert_eq!(count_files(&dir), 0, "临时文件必须清理干净");
        let _ = h.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn error_is_passed_through_and_temp_is_cleaned() {
        let (url, h) = spawn_fake_rc(Vec::new(), true);
        let dir = tmp("err");
        let client = RcClient::new(url, None, None);
        let err = fetch_to_temp(&client, "teldrive:", "missing.png", &dir)
            .await
            .expect_err("上游错误必须向上传递");
        assert_eq!(err.kind, RcErrorKind::Api, "应分类为 rclone 业务错误");
        assert!(
            err.detail.contains("backend boom"),
            "必须保留上游错误原文，实际: {}",
            err.detail
        );
        assert_eq!(count_files(&dir), 0, "失败路径也不能留下临时文件");
        let _ = h.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_names_do_not_collide() {
        let a = unique_tmp_name();
        let b = unique_tmp_name();
        assert_ne!(a, b);
        assert!(a.starts_with("rcgui-fetch-"));
    }
}
