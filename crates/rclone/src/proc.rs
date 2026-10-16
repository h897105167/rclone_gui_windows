//! rclone.exe 进程管理：探测、托管 rcd、cat 流式读取。

use crate::rc::RcClient;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};

/// 在文本行中提取 Windows 绝对路径（X:\ 或 X:/ 开头）
pub fn extract_windows_path(line: &str) -> Option<String> {
    let b = line.as_bytes();
    if b.len() < 3 {
        return None;
    }
    for i in 0..b.len() - 2 {
        if b[i].is_ascii_alphabetic() && b[i + 1] == b':' && (b[i + 2] == b'\\' || b[i + 2] == b'/') {
            return Some(line[i..].trim().trim_matches(['"', '\'']).to_string());
        }
    }
    None
}

pub fn generate_password() -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..24)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

/// 探测 rclone.exe：自定义路径 → 内嵌（由调用方通过 custom 传入）→ PATH → 常见安装位置
pub fn discover_exe(custom: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = custom {
        if !p.trim().is_empty() {
            let pb = PathBuf::from(p.trim());
            if pb.exists() {
                return Ok(pb);
            }
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join("rclone.exe");
            if cand.exists() {
                return Ok(cand);
            }
        }
    }
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let candidates = [
        format!(r"{local}\Rclone\rclone.exe"),
        r"C:\rclone\rclone.exe".to_string(),
        r"C:\Program Files\Rclone\rclone.exe".to_string(),
        format!(r"{local}\Microsoft\WinGet\Links\rclone.exe"),
        format!(r"{local}\scoop\shims\rclone.exe"),
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(anyhow!("未找到 rclone.exe，请在「设置」中手动指定路径"))
}

pub struct RcloneManager {
    pub client: RwLock<Arc<RcClient>>,
    child: Mutex<Option<Child>>,
    pub log_file: PathBuf,
}

impl RcloneManager {
    pub fn new(log_file: PathBuf) -> Self {
        Self {
            client: RwLock::new(Arc::new(RcClient::new("http://127.0.0.1:5572", None, None))),
            child: Mutex::new(None),
            log_file,
        }
    }

    /// `rclone config file` 输出中解析配置文件路径
    pub async fn detect_config_path(exe: &Path) -> Result<PathBuf> {
        let out = Command::new(exe)
            .args(["config", "file"])
            .output()
            .await
            .context("运行 rclone config file 失败")?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        for line in text.lines() {
            if let Some(p) = extract_windows_path(line) {
                if Path::new(&p).exists() {
                    return Ok(PathBuf::from(p));
                }
            }
        }
        // 最后兜底：默认位置
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        let def = PathBuf::from(format!(r"{appdata}\rclone\rclone.conf"));
        if def.exists() {
            return Ok(def);
        }
        Err(anyhow!("无法获取 rclone 配置文件路径"))
    }

    /// 启动托管 rcd 并等待就绪
    pub async fn start_managed(&self, exe: &Path, port: u16, user: &str, pass: &str) -> Result<()> {
        self.stop().await;
        let addr = format!("127.0.0.1:{port}");

        if let Some(parent) = self.log_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.log_file)
            .context("创建 rclone 日志文件失败")?;
        let err = log.try_clone()?;

        let child = Command::new(exe)
            .args([
                "rcd",
                "--rc-addr",
                &addr,
                "--rc-user",
                user,
                "--rc-pass",
                pass,
                "-vv",
                "--buffer-size",
                "4M",
                "--transfers",
                "2",
                "--checkers",
                "2",
                "--fs-cache-expire-duration",
                "5m",
            ])
            .stdout(log)
            .stderr(err)
            .stdin(Stdio::null())
            .spawn()
            .context("启动 rclone rcd 失败")?;
        *self.child.lock().await = Some(child);

        let url = format!("http://{addr}");
        let client = Arc::new(RcClient::new(url, Some(user.to_string()), Some(pass.to_string())));
        *self.client.write().await = client.clone();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("rclone rcd 启动超时（20s），请查看日志 {}", self.log_file.display());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            if client.version().await.is_ok() {
                return Ok(());
            }
            let mut guard = self.child.lock().await;
            if let Some(c) = guard.as_mut() {
                if let Ok(Some(_)) = c.try_wait() {
                    anyhow::bail!("rclone rcd 进程异常退出，请查看日志 {}", self.log_file.display());
                }
            } else {
                anyhow::bail!("rclone rcd 进程丢失");
            }
        }
    }

    pub async fn stop(&self) {
        if let Some(mut c) = self.child.lock().await.take() {
            let _ = c.kill().await;
        }
    }

    pub async fn is_managed_running(&self) -> bool {
        let mut guard = self.child.lock().await;
        match guard.as_mut() {
            Some(c) => c.try_wait().map(|s| s.is_none()).unwrap_or(false),
            None => false,
        }
    }

    /// 连接已有实例（验证可达）
    pub async fn attach(&self, url: &str, user: Option<String>, pass: Option<String>) -> Result<crate::types::VersionInfo> {
        let client = Arc::new(RcClient::new(url, user, pass));
        let info = client
            .version()
            .await
            .map_err(|e| anyhow!("连接已有 rclone 失败: {e}"))?;
        *self.client.write().await = client;
        Ok(info)
    }

    /// `rclone cat remote` 流式写入本地文件（用于直链代理缓存等）
    pub async fn cat_to_file(&self, exe: Option<&Path>, fs: &str, remote: &str, out_path: &Path) -> Result<()> {
        let exe = exe.ok_or_else(|| anyhow!("rclone.exe 路径未知"))?;
        // 拼接完整远端路径：remote 后端（"name:"）直接连；本地路径补分隔符
        let spec = if remote.is_empty() {
            fs.to_string()
        } else if fs.ends_with(':') {
            format!("{fs}{remote}")
        } else {
            format!("{fs}/{remote}")
        };
        let mut file = tokio::fs::File::create(out_path)
            .await
            .context("创建缓存文件失败")?;
        let mut child = Command::new(exe)
            .args(["cat", &spec])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn()
            .context("启动 rclone cat 失败")?;
        let mut out = child.stdout.take().ok_or_else(|| anyhow!("无 stdout"))?;
        tokio::io::copy(&mut out, &mut file).await?;
        let status = child.wait().await?;
        if !status.success() {
            let _ = tokio::fs::remove_file(out_path).await;
            anyhow::bail!("rclone cat 退出码 {:?}", status.code());
        }
        Ok(())
    }
}
