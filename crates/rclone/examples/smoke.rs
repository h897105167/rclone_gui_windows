//! rclone_client 冒烟测试：托管 rcd → 版本 → 本地 fs 增删复制 → 挂载列表。
//! 只操作系统临时目录，不触碰任何远程仓库。

use anyhow::Result;
use rclone_client::{discover_exe, local_fs, RcloneManager};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let exe = discover_exe(None)?;
    println!("[smoke] rclone.exe = {}", exe.display());

    let tmp = std::env::temp_dir().join(format!("rcgui-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    std::fs::write(tmp.join("hello.txt"), b"hello rclone gui")?;
    std::fs::create_dir_all(tmp.join("sub"))?;
    std::fs::write(tmp.join("sub").join("nested.txt"), b"nested")?;

    let log = tmp.join("rcd.log");
    let mgr = RcloneManager::new(log);
    let pass = rclone_client::generate_password();
    // 无论成败都停掉 rcd，避免孤儿进程
    let res = run(&mgr, &exe, &pass, &tmp).await;
    mgr.stop().await;
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

async fn run(mgr: &RcloneManager, exe: &std::path::Path, pass: &str, tmp: &std::path::Path) -> Result<()> {
    mgr.start_managed(exe, 5579, "gui", pass).await?;
    println!("[smoke] rcd started");

    let client = mgr.client.read().await.clone();
    let v = client.version().await?;
    println!("[smoke] version = {} ({}/{})", v.version, v.os, v.arch);

    let fs = local_fs(tmp);
    let entries = client.list(&fs, "", None).await?;
    println!("[smoke] list {:?} -> {} entries", tmp, entries.len());
    assert!(entries.iter().any(|e| e.name == "hello.txt"));

    client.mkdir(&fs, "newdir").await?;
    client
        .copyfile(&fs, "hello.txt", &fs, "newdir/hello2.txt")
        .await?;
    let entries = client.list(&fs, "newdir", None).await?;
    assert!(entries.iter().any(|e| e.name == "hello2.txt"));
    println!("[smoke] mkdir + copyfile ok");

    client
        .movefile(&fs, "newdir/hello2.txt", &fs, "sub/renamed.txt")
        .await?;
    let entries = client.list(&fs, "sub", None).await?;
    assert!(entries.iter().any(|e| e.name == "renamed.txt"));
    println!("[smoke] movefile ok");

    client.deletefile(&fs, "sub/renamed.txt").await?;
    client.rmdir(&fs, "newdir").await?; // hello2 已被移走，newdir 已空
    println!("[smoke] delete ok");

    // cat 流式读取
    let out = tmp.join("cat-out.txt");
    mgr.cat_to_file(Some(&exe), &fs, "hello.txt", &out).await?;
    assert_eq!(std::fs::read(&out)?, b"hello rclone gui");
    println!("[smoke] cat ok");

    // 真实挂载/卸载验证：mountPoint="*" 自动分配盘符
    client
        .mount(&fs, "*", json!({"vfs_cache_mode": "minimal"}))
        .await?;
    let mounts = client.mounts().await?;
    assert!(!mounts.is_empty(), "应有活动挂载");
    let mp = mounts[0].mount_point.clone();
    println!("[smoke] mounted at {mp}");
    let through = std::fs::read_dir(&mp)?.count();
    assert!(through >= 1, "挂载盘内应有文件");
    let content = std::fs::read(format!("{mp}\\hello.txt"))?;
    assert_eq!(content, b"hello rclone gui");
    client.unmount(&mp).await?;
    println!("[smoke] mount/unmount ok");

    let remotes = client.list_remotes().await?;
    println!("[smoke] remotes = {:?}", remotes);

    // publiclink 在本地 fs 上预期失败，忽略
    let _ = client.publiclink(&fs, "hello.txt").await;

    // stats
    let stats = client.core_stats().await?;
    println!("[smoke] stats speed = {}", stats.speed);

    println!("[smoke] ALL OK");
    Ok(())
}
