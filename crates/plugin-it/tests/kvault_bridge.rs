//! 宿主通用 remote 协议 ↔ 真 K-Vault 插件 wasm 的跨仓库行为测试。
//!
//! 这里跑的是插件仓库发布出去的那份 `main.wasm`，经 `plugin_host::invoke_with`
//! 走的是主程序用的同一个入口；宿主那一半由 `crates/plugin-it/src/host.rs` 的替身
//! 按同一份参数名回答。任何一侧偷偷改了协议，红的都是这里。
//!
//! 没设 `RCGUI_IT_WASM` 时每个测试打印一句原因后直接返回：没有 wasm 的机器上
//! `cargo test --workspace` 仍然是绿的。CI 里设 `RCGUI_IT_REQUIRED=1`，那时缺输入
//! 就是失败——不能靠「全是跳过」假装通过。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use plugin_host::config::REMOTE_OPS;
use plugin_host::Allowed;
use plugin_it::{expect_error, kvault_harness, plain_webdav_harness, PLUGIN_ID, SKIP_NOTE};
use serde_json::{json, Value};

/// 插件把业务失败写成 `Ok` 里的 `status`/`toast`，不是 `Err`。
/// 所以断言统一把回包压成字符串再找片段，两种形状都能覆盖。
fn text_of(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn items_of(v: &Value) -> Vec<(String, bool)> {
    v.get("items")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|it| {
                    (
                        it.get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        it.get("is_dir").and_then(Value::as_bool).unwrap_or(false),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 面板上某个按钮此刻能不能点。
///
/// 缺字段一律当「不能点」：宿主正是这么理解的，测试如果缺省成 `true`，插件哪天漏发
/// `enabled` 就会是绿的，而用户看到的是一个永远灰着的按钮。
fn flag_of(body: &Value, button: &str) -> bool {
    body.get("enabled")
        .and_then(|e| e.get(button))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 面板上某个输入框回显的值。
fn field_of(body: &Value, key: &str) -> String {
    body.get("values")
        .and_then(|v| v.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 写一个小文件并返回路径；测试结束后不清理，临时目录里的几十字节不值得加 teardown。
///
/// 名字前面挂一个进程内序号：同一个测试二进制里的用例是并行跑的，两个用例写同一个
/// 文件名时，后写的那个会把前一个刚登记授权的文件改掉，授权那边看到长度或修改时间
/// 变了就报「文件在挑选之后被改过」——错的不是被测代码，是这个助手。
fn temp_file(name: &str, body: &[u8]) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!("plugin-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录失败");
    let path = dir.join(format!("{}-{}", SEQ.fetch_add(1, Ordering::SeqCst), name));
    std::fs::write(&path, body).expect("写临时文件失败");
    path
}

#[tokio::test]
async fn the_fixture_is_the_shipped_plugin_package() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let cfg = h.fixture.config_json().expect("读插件 config.json 失败");
    assert_eq!(cfg.id, PLUGIN_ID, "config.json 的 id 不是 kvault");
    // 大小下限只是防止「拿了个空壳或占位文件」：真的 wasm 有几 MB，200 KB 已经
    // 比任何手写的占位大得多，又不会因为插件瘦身而误报。
    let size = std::fs::metadata(&h.fixture.wasm)
        .expect("读 wasm 元数据失败")
        .len();
    assert!(
        size > 200 * 1024,
        "wasm 只有 {size} 字节，不像插件真包（是不是拿错了文件？）"
    );
    // wasm 自己报的 id 必须与 config.json 一致：两个文件来自不同版本时，
    // 后面所有断言都在测一个不存在的东西。
    let manifest = h.manifest().await.expect("读插件 manifest 失败");
    assert_eq!(manifest.get("id").and_then(Value::as_str), Some(PLUGIN_ID));
}

#[tokio::test]
async fn the_manifest_agrees_with_the_shipped_config() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let manifest = h.manifest().await.expect("读插件 manifest 失败");
    assert_eq!(manifest.get("id").and_then(Value::as_str), Some(PLUGIN_ID));
    let version = manifest
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let cfg = h.fixture.config_json().expect("读插件 config.json 失败");
    // 两处版本必须一致：不同步时，商店显示的和实际跑的不是同一个东西。
    assert_eq!(
        version, cfg.version,
        "wasm 里的 version 与 config.json 不一致"
    );
    assert!(!version.is_empty(), "版本号是空的");
}

#[tokio::test]
async fn the_remote_root_lists_the_real_root_not_the_upload_folder() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let got = h
        .call(
            "remote.list",
            json!({ "remote": "vault", "path": "", "sort": "name" }),
        )
        .await
        .expect("remote.list 调用失败");
    let items = items_of(&got);
    let names: Vec<String> = items.iter().map(|(n, _)| n.clone()).collect();

    // 主程序的文件页在远程根上，面包屑第一项就是远程名，所以这里必须列远程根：
    // 假站点的三条记录都在 `album` 下（`album` 与 `album/2026`），根这一层只有
    // 一个目录 `album`。以前空路径回退到上传目录，界面写着远程根、内容却是
    // 上传目录那一层，位置与内容对不上。
    assert_eq!(
        names,
        vec!["album".to_string()],
        "远程根这一层应该只有 album 目录：{}",
        text_of(&got)
    );
    assert!(items[0].1, "album 应该是目录：{}", text_of(&got));

    let hits = h.site.hits();
    let lists = hits
        .iter()
        .filter(|h| h.starts_with("GET /api/v1/files"))
        .count();
    assert_eq!(lists, 2, "翻页次数不对：{hits:?}");
    // 列表只走站点 API。真去读了 DAV，说明有路径绕过了协议。
    assert!(
        !hits.iter().any(|h| h.starts_with("GET /dav")),
        "列表读取不该走 DAV：{hits:?}"
    );
}

#[tokio::test]
async fn a_subfolder_listing_only_shows_files_under_that_folder() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let got = h
        .call("remote.list", json!({ "remote": "vault", "path": "album" }))
        .await
        .expect("remote.list 调用失败");
    let names: Vec<String> = items_of(&got).into_iter().map(|(n, _)| n).collect();
    assert!(
        names.contains(&"2026".to_string()),
        "album 下应该有子目录 2026：{}",
        text_of(&got)
    );
    assert!(
        !names.contains(&"big.bin".to_string()),
        "album/2026 里的文件不该出现在 album 这一层：{}",
        text_of(&got)
    );
}

#[tokio::test]
async fn upload_sends_the_picked_bytes_and_the_idempotency_key() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let bytes: &[u8] = b"\x89PNG\r\n\x1a\nfake-bytes";
    let path = temp_file("cover.png", bytes);
    let key = "remote-0-cover.png";
    // 宿主先登记这次挑选：没授权，`http.upload` 一个字节都发不出去。
    plugin_host::upload::grant(PLUGIN_ID, key, &path).expect("登记授权失败");

    let got = h
        .call(
            "remote.upload",
            json!({
                "remote": "vault",
                "path": "album",
                "file_name": "cover.png",
                "file_key": key,
            }),
        )
        .await
        .expect("remote.upload 调用失败");
    assert!(
        got.get("name").and_then(Value::as_str).is_some(),
        "上传成功回包里应该有文件名：{}",
        text_of(&got)
    );

    let uploads = h.site.uploads();
    assert_eq!(uploads.len(), 1, "站点应该恰好收到一次上传");
    let up = &uploads[0];
    assert_eq!(up.path, "/api/v1/upload");
    // 端到端比对字节：这是「挑中的那个文件真的发出去了」的唯一证据。
    assert_eq!(up.file_bytes, bytes, "收到的字节和被挑中的文件不一致");
    // 同一次挑选重发不该在站点上变成两个文件。
    assert!(
        up.idempotency_key.starts_with("kvault-"),
        "缺少幂等键或前缀不对：{:?}",
        up.idempotency_key
    );
    // 文本字段必须排在文件段之前，服务端才能在收字节时就知道该往哪儿放。
    assert!(
        up.fields_before_file(),
        "表单字段排在了文件段之后：field_offset={} file_offset={}",
        up.field_offset,
        up.file_offset
    );
    assert!(
        !up.fields.is_empty(),
        "上传请求里没有任何文本字段，插件漏了站点要的参数"
    );
    plugin_host::upload::revoke(PLUGIN_ID, key);
}

/// 回包必须带着站点给的直链和去重标记。
///
/// 主程序的 `rclone.upload_file` 拿到这个回包后，`link` 非空就把它当图片地址给桌面端，
/// 空了才会退到「本机代理」那条路——那串 `127.0.0.1` 在别的设备上打不开。
/// 所以站点回了直链、插件却没往外递，用户在别处就看不到图。
#[tokio::test]
async fn the_upload_reply_carries_the_site_link_and_the_dedupe_flag() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let path = temp_file("cover.png", b"\x89PNG\r\n\x1a\nfake-bytes");
    // 主程序给每次上传单独一格授权（`cap-<进程号>-<序号>`），这里用不重样的键，
    // 免得并行跑的另一个用例把这次的授权顶掉——那会变成「发出去的是别人的文件」。
    let key = "it-reply-link";
    plugin_host::upload::grant(PLUGIN_ID, key, &path).expect("登记授权失败");

    let got = h
        .call(
            "remote.upload",
            json!({
                "remote": "vault",
                "path": "album",
                "file_name": "cover.png",
                "file_key": key,
            }),
        )
        .await
        .expect("remote.upload 调用失败");

    let link = got.get("link").and_then(Value::as_str).unwrap_or("");
    // 站点在测试里回的是相对地址 `/f/a.png`，插件补全后才是能直接打开的链接。
    assert!(
        link.ends_with("/f/a.png"),
        "直链应该来自站点的回复并补全成完整地址：{}",
        text_of(&got)
    );
    assert!(
        link.contains("://"),
        "直链没补全，桌面端拿到的是相对路径：{link}"
    );
    assert_eq!(
        got.get("deduped").and_then(Value::as_bool),
        Some(false),
        "站点说这次不是重复文件，回包就该照实说：{}",
        text_of(&got)
    );
    plugin_host::upload::revoke(PLUGIN_ID, key);
}

/// 没给路径的上传仍然落在这条远程配好的目录里。
///
/// 图床选完文件就调 `remote.upload`，此时界面没有「当前目录」可传。所以空路径
/// 对上传必须回退到配好的目录（这里假宿主配的是 `album`，真机上就是「上传图片
/// 路径」那种设置）。列目录那边相反：空路径必须真的是远程根，否则文件页顶部
/// 写着远程根、列出来的却是上传目录那一层。
#[tokio::test]
async fn an_upload_without_a_path_still_lands_in_the_configured_folder() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let path = temp_file("cover.png", b"\x89PNG\r\n\x1a\nfake-bytes");
    let key = "it-default-folder";
    plugin_host::upload::grant(PLUGIN_ID, key, &path).expect("登记授权失败");

    h.call(
        "remote.upload",
        json!({
            "remote": "vault",
            "path": "",
            "file_name": "cover.png",
            "file_key": key,
        }),
    )
    .await
    .expect("remote.upload 调用失败");

    let uploads = h.site.uploads();
    assert_eq!(uploads.len(), 1, "站点应该恰好收到一次上传");
    let folder = uploads[0]
        .fields
        .iter()
        .find(|(name, _)| name == "folderPath")
        .map(|(_, value)| value.as_str());
    assert_eq!(
        folder,
        Some("album"),
        "空路径上传没有落到配置的上传目录，图床的默认目录变了"
    );
    plugin_host::upload::revoke(PLUGIN_ID, key);
}

/// 传了子目录就传到子目录，不能被默认目录顶掉。
#[tokio::test]
async fn an_upload_inside_a_subfolder_keeps_that_subfolder() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let path = temp_file("cover.png", b"\x89PNG\r\n\x1a\nfake-bytes");
    let key = "it-subfolder";
    plugin_host::upload::grant(PLUGIN_ID, key, &path).expect("登记授权失败");

    h.call(
        "remote.upload",
        json!({
            "remote": "vault",
            "path": "album/2026",
            "file_name": "cover.png",
            "file_key": key,
        }),
    )
    .await
    .expect("remote.upload 调用失败");

    let uploads = h.site.uploads();
    assert_eq!(uploads.len(), 1, "站点应该恰好收到一次上传");
    let folder = uploads[0]
        .fields
        .iter()
        .find(|(name, _)| name == "folderPath")
        .map(|(_, value)| value.as_str());
    assert_eq!(
        folder,
        Some("album/2026"),
        "子目录上传被改到了别的位置：{:?}",
        uploads[0].fields
    );
    plugin_host::upload::revoke(PLUGIN_ID, key);
}

#[tokio::test]
async fn upload_without_a_picked_file_is_refused_before_any_request() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let got = h
        .call(
            "remote.upload",
            json!({ "remote": "vault", "path": "album" }),
        )
        .await
        .expect("remote.upload 本身不该失败");
    let body = text_of(&got);
    assert!(
        got.get("status").and_then(Value::as_str).is_some(),
        "没挑文件时应该有一句可读的拒绝：{body}"
    );
    assert!(h.site.uploads().is_empty(), "被拒绝了却还是发了请求");
}

#[tokio::test]
async fn delete_reports_what_went_and_what_was_already_gone() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let got = h
        .call(
            "remote.delete",
            json!({
                "remote": "vault",
                "path": "album",
                "items": [{ "name": "photo.png" }, { "name": "already-gone.txt" }],
            }),
        )
        .await
        .expect("remote.delete 调用失败");

    let deleted_ids = h.site.deleted_ids();
    assert_eq!(
        deleted_ids,
        vec!["f1".to_string()],
        "应该只删掉 photo.png 那一条：{deleted_ids:?}"
    );
    let body = text_of(&got);
    assert!(body.contains("photo.png"), "回包里没提到删掉的文件：{body}");
    assert!(
        body.contains("already-gone.txt"),
        "回包里没提到没找到的文件：{body}"
    );
}

#[tokio::test]
async fn adopt_legacy_fills_only_the_slots_that_are_still_empty() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    // 插件里已经有一份设置（用户在插件面板里填过的），旧数据只能补空缺。
    //
    // 形状要和插件真实存的一致：宿主只给插件看它自己那一格，那一格里的键就叫 `site`
    // （插件读的是 `kv["site"]`）。再套一层插件 id，插件就会读到一张空表，把旧地址、
    // 旧令牌全部盖上去——那时红的是夹具，不是迁移。
    h.host.put_kv(
        "site",
        json!({ "base": "https://new.example.com", "token": "fresh-token" }),
    );
    // 旧库的形状（`crates/app/src/kvault.rs` 被删之前的样子）：`kvault` 命名空间里
    // 一个 `site` 键放开店设置，其余键是「远程名 → {token}」；`kvault_tokens` 里
    // 全是后者。插件认的正是这个形状——键名 `site` 就是整站设置，别把它改叫
    // 命名空间名，那样插件会当成一条叫 kvault 的远程，站点设置原样丢掉。
    let legacy = json!({
        "site": {
            "base": "https://old.example.com",
            "token": "old-token",
            "folder": "album",
            "storage": "telegram",
        },
        "vault": { "token": "remote-token" },
    });

    let got = h
        .call(
            "adopt_legacy",
            json!({ "namespace": "kvault_tokens", "legacy": legacy }),
        )
        .await
        .expect("adopt_legacy 调用失败");
    assert!(
        got.get("stored_keys").and_then(Value::as_array).is_some(),
        "回包里应该报这次写了哪些键：{}",
        text_of(&got)
    );

    let kept = h.host.kv_snapshot();
    let site = kept.get("site").cloned().unwrap_or(Value::Null);
    // 已有的两项必须原样留着：覆盖是最坏的失败方式，用户的令牌会悄悄失效。
    assert_eq!(
        site.get("token").and_then(Value::as_str),
        Some("fresh-token"),
        "已有令牌被旧数据覆盖了：{}",
        text_of(&site)
    );
    assert_eq!(
        site.get("base").and_then(Value::as_str),
        Some("https://new.example.com"),
        "已有站点地址被旧数据覆盖了：{}",
        text_of(&site)
    );
    // 旧数据里新的一格没有的字段，补上了。
    assert_eq!(
        site.get("folder").and_then(Value::as_str),
        Some("album"),
        "旧设置里的目录没迁过来：{}",
        text_of(&site)
    );
    // 另一条远程的令牌也搬了过来，而且只搬 token 这一项。
    // 插件自己的 kv 是一张扁表（宿主只给它看自己那一格），远程名就是表里的键。
    let remote = kept.get("vault").cloned().unwrap_or(Value::Null);
    assert_eq!(
        remote.get("token").and_then(Value::as_str),
        Some("remote-token"),
        "远程令牌没迁过来：{}",
        text_of(&remote)
    );
}

#[tokio::test]
async fn adopt_legacy_refuses_garbage_instead_of_wiping_settings() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    h.host
        .put_kv("site", json!({ "base": "https://keep.example.com" }));
    let got = h
        .call(
            "adopt_legacy",
            json!({ "namespace": "kvault_tokens", "legacy": "not-an-object" }),
        )
        .await
        .expect("adopt_legacy 本身不该失败");
    assert!(
        got.get("status").and_then(Value::as_str).is_some(),
        "旧数据形状不对时应该拒绝并说明：{}",
        text_of(&got)
    );
    let kept = h.host.kv_snapshot();
    assert_eq!(
        kept.get("site")
            .and_then(|site| site.get("base"))
            .and_then(Value::as_str),
        Some("https://keep.example.com"),
        "拒绝时手里的设置被动了"
    );
}

/// 早期版本写下的设置形状（只有站点地址、令牌、目录三样）现在照样读得懂，读完一个键都不许动。
///
/// **这条验的是设置文件形状的兼容，不是二进制跨版本升级。**插件不记「设置版本号」，
/// 它按形状读 `kv["site"]`、缺的字段取默认值，所以升级真正的风险不是「迁移没跑」，
/// 而是「新版本把老形状读成空，再把一坨半成品写回去」。
///
/// 真跨版本（旧 wasm 写数据、新 wasm 读）需要两个版本的二进制同时在场，这里只有当前
/// 那一个；拿改 config 版本号来假装旧版是伪造，不做。见 `docs/REPORT-ui-core-fixes.md`
/// 局限一节。
#[tokio::test]
async fn settings_without_the_newer_fields_are_read_as_written() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    // 早期只写站点根（不带 `/dav`）、令牌、目录；`storage`/`dedup` 是后来才加的字段，
    // 缺了它们必须走默认值，而不是读成空设置。
    let base = h.site.base();
    h.host.put_kv(
        "site",
        json!({ "base": base, "token": "test-token", "folder": "album" }),
    );
    let before = h.host.kv_snapshot();
    // 列目录走远程根：设置里的 `folder` 是上传的默认位置，不再是浏览的起点。
    let got = h
        .call("remote.list", json!({ "remote": "", "path": "" }))
        .await
        .expect("只有早期字段的设置，应当照旧能用");
    let rows = items_of(&got);
    assert!(
        rows.iter().any(|(name, is_dir)| name == "album" && *is_dir),
        "早期设置下远程根没列出来：{rows:?}"
    );

    // 上传仍然用上设置里的目录：旧记录的 `folder` 必须在传给上传时读出来。
    let path = temp_file("cover.png", b"\x89PNG\r\n\x1a\nfake-bytes");
    let key = "it-legacy-settings";
    plugin_host::upload::grant(PLUGIN_ID, key, &path).expect("登记授权失败");
    h.call(
        "remote.upload",
        json!({
            "remote": "",
            "path": "",
            "file_name": "cover.png",
            "file_key": key,
        }),
    )
    .await
    .expect("只有早期字段的设置，上传也应当照旧能用");
    let uploads = h.site.uploads();
    assert_eq!(uploads.len(), 1, "站点应该恰好收到一次上传");
    let folder = uploads[0]
        .fields
        .iter()
        .find(|(name, _)| name == "folderPath")
        .map(|(_, value)| value.as_str());
    assert_eq!(
        folder,
        Some("album"),
        "早期设置里的目录没被用上，上传落到了别处：{:?}",
        uploads[0].fields
    );
    plugin_host::upload::revoke(PLUGIN_ID, key);

    assert_eq!(
        h.host.kv_snapshot(),
        before,
        "只是列了一次目录、传了一个文件，用户存的设置却被改写了"
    );
}

#[tokio::test]
async fn a_plain_webdav_remote_is_never_treated_as_a_site() {
    let Some(h) = plain_webdav_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let res = h
        .call("remote.list", json!({ "remote": "vault", "path": "" }))
        .await;
    let message = match res {
        Ok(v) => text_of(&v),
        Err(e) => e.to_string(),
    };
    assert!(!message.trim().is_empty(), "被拒绝了却没有任何说明");
    // 普通 WebDAV 没有站点地址；一旦这里冒出 /api/ 请求，就等于把用户的
    // 另一个网盘当成了 K-Vault 站点在用。
    let hits = h.site.hits();
    assert!(
        hits.iter().all(|hit| !hit.starts_with("GET /api/")),
        "普通 WebDAV 被当成站点去调了 API：{hits:?}"
    );
}

#[tokio::test]
async fn a_plugin_without_the_capability_group_is_refused() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let none = Allowed::from_groups(&[]);
    // 拒绝发生在第二个 need 上：第一个 `kv.get_all` 走的是宿主的旧通道，不查能力组；
    // 第二个 `rclone.call` 才被 `Allowed::check` 挡住。所以文案里点名的是 rclone 组，
    // 这里就按真实文案断言——传空片段等于断言「随便什么错误都行」，那才是掩盖问题。
    let err = expect_error(
        h.call_allowed(
            &none,
            "remote.list",
            json!({ "remote": "vault", "path": "" }),
        )
        .await,
        &["rclone"],
    );
    assert!(!err.is_empty(), "空能力组应该让调用失败");
    assert!(h.site.hits().is_empty(), "没有能力却发出了 HTTP 请求");
}

/// 清单里声明的东西，必须是宿主真会用的东西。
///
/// 宿主判断「这条远程归谁管」靠的是清单里的 `marker`，把远程交给插件靠的是
/// `ops`，接管旧设置靠的是 `adopt_legacy`。任何一个漏写，表现都不是报错，
/// 而是那条远程悄悄退回原生 WebDAV（或者旧设置永远搬不过来）——所以这里逐项核对。
#[tokio::test]
async fn the_manifest_declares_everything_the_host_router_reads() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let cfg = h.fixture.config_json().expect("config.json 读不出来");

    let rt = cfg
        .remote_types
        .first()
        .expect("没有声明 remote_types：宿主的新建远程下拉框里不会出现这个类型");
    let marker = rt.marker_key();
    assert!(!marker.trim().is_empty(), "标记键是空的，谁也算不出归属");
    // 宿主只把非空的标记值算作标记（off/false/0/空串都不算），所以 defaults
    // 里必须真的给这个键写了一个值：否则新建出来的远程没有标记，宿主认不出它。
    let marked = rt.defaults.get(&marker).map(String::as_str).unwrap_or("");
    assert!(
        !marked.trim().is_empty(),
        "defaults 里没有给标记键 {marker} 一个非空值，新建的远程不会被认作这个类型"
    );

    // ops 的名字是宿主与插件之间的接口，写错一个就永远路由不到；
    // 少写 remote.list/upload/delete 里的任意一个，对应动作会退回原生后端。
    for op in &rt.ops {
        assert!(
            REMOTE_OPS.contains(&op.as_str()),
            "{op} 不是宿主认识的操作名，可用的是 {REMOTE_OPS:?}"
        );
    }
    for need in ["remote.list", "remote.upload", "remote.delete"] {
        assert!(
            rt.supports(need),
            "清单没声明 {need}，宿主的对应操作会退回原生 WebDAV"
        );
    }

    // 旧设置靠这个字段搬过来；空表意味着用户以前存的令牌和站点地址永远留在旧键里。
    assert!(
        !cfg.adopt_legacy.is_empty(),
        "adopt_legacy 是空的，旧版本的设置不会被接管"
    );
    for ns in &cfg.adopt_legacy {
        assert!(
            !ns.trim().is_empty() && !ns.contains('/') && !ns.contains('\\'),
            "命名空间 {ns:?} 不是合法的存储键名"
        );
    }

    // wasm 报回来的版本必须和这份 config.json 一致：拿错包时立刻暴露，
    // 而不是等到用户装上以后面板对不上。
    let m = h.manifest().await.expect("wasm 没有返回 manifest");
    assert_eq!(m["version"].as_str(), Some(cfg.version.as_str()));
    assert_eq!(m["id"].as_str(), Some(cfg.id.as_str()));
}

/// 用户每敲一个字符，宿主就会重问一次面板；这一问必须看见「正在输入的那份表单」。
///
/// 输入框里的字存在插件命名空间的 `form` 键里，插件只有把这份表单叠到已保存的设置上，
/// 按钮才会亮。曾经这里只读已保存的那份，于是「先填站点地址」永远成立、`测试连接`
/// 永远是灰的——而能保存设置的只有测试连接之后的动作，用户就永远走不出去。
#[tokio::test]
async fn typing_the_site_turns_the_test_button_on() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let base = h.site.base();
    let before = h.host.kv_snapshot();
    // 一个字都还没保存：这份表单就是用户此刻输入框里的内容。
    let body = h
        .call(
            "state",
            json!({ "form": { "base": base.clone(), "token": "test-token" } }),
        )
        .await
        .expect("填了站点地址，面板应当能重拉出来");

    assert!(
        flag_of(&body, "test"),
        "填了站点地址却还点不了测试连接：{body}"
    );
    assert!(
        flag_of(&body, "create"),
        "地址和令牌都在，建远程却还是灰的：{body}"
    );
    assert_eq!(
        field_of(&body, "base"),
        base,
        "面板回显的地址与输入框里输入的不一致：{body}"
    );
    assert_eq!(
        h.host.kv_snapshot(),
        before,
        "只是重拉了一次面板，用户存的设置却被改写了"
    );
}

/// 地址框被清空之后，面板不能还停在「已就绪」。
///
/// 已保存的设置仍然是好的，所以这条只能靠表单说得算：用户删掉了字，按钮就必须跟着灰掉，
/// 否则点下去测的是一个他已经不打算用的地址。
#[tokio::test]
async fn clearing_the_address_puts_the_buttons_back_to_waiting() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let base = h.site.base();
    h.host.put_kv(
        "site",
        json!({ "base": base, "token": "test-token", "folder": "album" }),
    );
    let body = h
        .call("state", json!({ "form": { "base": "" } }))
        .await
        .expect("清空地址之后，面板仍然要能重拉");

    assert!(
        !flag_of(&body, "test"),
        "地址已经清空，测试连接却还能点：{body}"
    );
    assert_eq!(
        field_of(&body, "base"),
        "",
        "输入框已经清空，面板却把旧地址塞了回去：{body}"
    );
}

/// 存过设置之后，正在输入的那份仍然说了算。
///
/// 这是「改完地址还没点保存」的常态：面板回显必须是用户刚敲的字，不能是上次存下来的那份。
#[tokio::test]
async fn a_saved_site_never_overwrites_what_is_being_typed() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let saved = h.site.base();
    h.host.put_kv(
        "site",
        json!({ "base": saved, "token": "test-token", "folder": "album" }),
    );
    let typed = "https://new.example.com";
    let body = h
        .call(
            "state",
            json!({ "form": { "base": typed, "token": "test-token" } }),
        )
        .await
        .expect("改了地址之后，面板仍然要能重拉");

    assert_eq!(
        field_of(&body, "base"),
        typed,
        "面板回显的是旧设置，用户刚敲的地址被覆盖了：{body}"
    );
    assert!(
        flag_of(&body, "test"),
        "地址已经改成能用的样子，测试连接却还是灰的：{body}"
    );
}

/// 宿主把插件托管的远程整个交给插件之后，字节只能走站点的上传接口。
///
/// 用户报的 `403 Forbidden: this WebDAV endpoint is read-only` 就是反例：主程序以前
/// 一律用 rclone 的 copyfile，对这种远程等于往只读的 DAV 面发 PUT。这里按宿主真实的
/// 请求形状（`remote`/`path`/`file_name`/`file_key`）走一遍，检查两件事：一条 PUT 都没有，
/// 且字节、文件名、目标目录三项都对得上。
#[tokio::test]
async fn a_host_routed_upload_never_touches_the_read_only_dav_surface() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let bytes: &[u8] = b"\x89PNG\r\n\x1a\nhost-routed";
    let path = temp_file("routed.png", bytes);
    // 主程序的授权键是 `cap-<pid>-<序号>`，这里照同样的形状取一个。
    let key = "cap-4242-0";
    plugin_host::upload::grant(PLUGIN_ID, key, &path).expect("登记授权失败");

    let got = h
        .call(
            "remote.upload",
            json!({
                "remote": "vault",
                "path": "album/sub",
                "file_name": "routed.png",
                "file_key": key,
            }),
        )
        .await
        .expect("remote.upload 调用失败");

    let hits = h.site.hits();
    assert!(
        !hits.iter().any(|line| line.starts_with("PUT ")),
        "字节被发到只读的 DAV 面上了：{hits:?}"
    );
    assert!(
        hits.iter().any(|line| line == "POST /api/v1/upload"),
        "站点没收到上传接口的请求：{hits:?}"
    );

    let uploads = h.site.uploads();
    assert_eq!(uploads.len(), 1, "应该恰好一次上传");
    let up = &uploads[0];
    assert_eq!(up.file_bytes, bytes, "站点收到的字节和挑中的文件不一致");
    assert_eq!(up.file_name, "routed.png", "文件名对不上：{}", up.file_name);
    let folder = up
        .fields
        .iter()
        .find(|(name, _)| name == "folderPath")
        .map(|(_, value)| value.clone());
    assert_eq!(
        folder.as_deref(),
        Some("album/sub"),
        "上传目标目录不对：{:?}",
        up.fields
    );

    let link = got.get("link").and_then(Value::as_str).unwrap_or("");
    assert!(
        link.starts_with("http://") && link.contains("://"),
        "桌面端拿到的应该是补全过的直链：{link}"
    );
    plugin_host::upload::revoke(PLUGIN_ID, key);
}

/// 连着两次上传各用各的授权：第二次不会顶掉第一次，字节也不会串。
///
/// 授权表按 `(插件, 键)` 记，键一旦重复，后登记的会把前一条覆盖掉——那时第二次上传
/// 发出去的是第一个文件，撤销时撤的也是别人那格。这里连着走两遍，逐次核对字节。
#[tokio::test]
async fn two_uploads_in_a_row_keep_their_own_files() {
    let Some(h) = kvault_harness().await else {
        eprintln!("{SKIP_NOTE}");
        return;
    };
    let first: &[u8] = b"\x89PNG\r\n\x1a\nfirst";
    let second: &[u8] = b"\x89PNG\r\n\x1a\nsecond";
    let path_a = temp_file("first.png", first);
    let path_b = temp_file("second.png", second);
    let key_a = "cap-7-0";
    let key_b = "cap-7-1";
    plugin_host::upload::grant(PLUGIN_ID, key_a, &path_a).expect("登记第一条授权失败");
    plugin_host::upload::grant(PLUGIN_ID, key_b, &path_b).expect("登记第二条授权失败");
    assert!(
        plugin_host::upload::waiting(PLUGIN_ID, key_a),
        "后登记的授权顶掉了前一条"
    );

    for (key, name) in [(key_a, "first.png"), (key_b, "second.png")] {
        h.call(
            "remote.upload",
            json!({
                "remote": "vault",
                "path": "album",
                "file_name": name,
                "file_key": key,
            }),
        )
        .await
        .expect("remote.upload 调用失败");
        assert!(
            !plugin_host::upload::waiting(PLUGIN_ID, key),
            "{key} 发完就该作废，留在表里下次会被别人拿去用"
        );
    }

    let uploads = h.site.uploads();
    assert_eq!(uploads.len(), 2, "站点应该收到两次上传");
    assert_eq!(uploads[0].file_bytes, first, "第一次上传的字节不对");
    assert_eq!(uploads[0].file_name, "first.png");
    assert_eq!(uploads[1].file_bytes, second, "第二次上传的字节不对");
    assert_eq!(uploads[1].file_name, "second.png");
}
