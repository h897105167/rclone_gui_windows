//! 宿主通用 remote 协议 ↔ 真 K-Vault 插件 wasm 的跨仓库行为测试。
//!
//! 这里跑的是插件仓库发布出去的那份 `main.wasm`，经 `plugin_host::invoke_with`
//! 走的是主程序用的同一个入口；宿主那一半由 `crates/plugin-it/src/host.rs` 的替身
//! 按同一份参数名回答。任何一侧偷偷改了协议，红的都是这里。
//!
//! 没设 `RCGUI_IT_WASM` 时每个测试打印一句原因后直接返回：没有 wasm 的机器上
//! `cargo test --workspace` 仍然是绿的。CI 里设 `RCGUI_IT_REQUIRED=1`，那时缺输入
//! 就是失败——不能靠「全是跳过」假装通过。

use std::path::{Path, PathBuf};

use plugin_host::config::REMOTE_OPS;
use plugin_host::Allowed;
use plugin_it::{
    expect_error, kvault_harness, plain_webdav_harness, Harness, MockHost, MockSite, PLUGIN_ID,
    SKIP_NOTE,
};
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

/// 写一个小文件并返回路径；测试结束后不清理，临时目录里的几十字节不值得加 teardown。
fn temp_file(name: &str, body: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("plugin-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录失败");
    let path = dir.join(name);
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
async fn listing_walks_every_page_and_puts_folders_first() {
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

    // 站点第一页只给两条并说还有下一页（photo.png、big.bin），第二页才给
    // notes.txt：根目录能同时看见 photo.png 和 notes.txt，就说明翻页真的走了。
    assert_eq!(
        names,
        vec![
            "album".to_string(),
            "notes.txt".to_string(),
            "photo.png".to_string()
        ],
        "根目录应该是先目录后文件、且覆盖两页：{}",
        text_of(&got)
    );
    assert!(items[0].1, "目录没有排在文件前面：{}", text_of(&got));

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
    h.host.put_kv(
        PLUGIN_ID,
        json!({ "base": "https://new.example.com", "token": "fresh-token" }),
    );
    let legacy = json!({
        "kvault": {
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
    let site = kept.get(PLUGIN_ID).cloned().unwrap_or(Value::Null);
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
        .put_kv(PLUGIN_ID, json!({ "base": "https://keep.example.com" }));
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
        kept.get(PLUGIN_ID)
            .and_then(|v| v.get("base"))
            .and_then(Value::as_str),
        Some("https://keep.example.com"),
        "拒绝时手里的设置被动了"
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
    let err = expect_error(
        h.call_allowed(
            &none,
            "remote.list",
            json!({ "remote": "vault", "path": "" }),
        )
        .await,
        &[],
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

/// 让三个类型在只用到其中一部分时也不产生「未使用」告警。
#[allow(dead_code)]
fn _exports(_: &Harness, _: &MockHost, _: &MockSite, _: &Path) {}
