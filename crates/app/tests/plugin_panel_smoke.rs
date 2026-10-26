//! 面板协议冒烟测试：拿**真正随包发出去的** wasm，在真宿主（wasmtime）里跑一遍
//! `state` 动作，断言它回了一个面板能直接渲染的 `result.body`。
//!
//! 为什么要有这层测试：插件在它自己的仓库里跑单元测试时用的是 native 走 `handle()`，
//! 真正上线是 wasm + 宿主协议（stdin 一行 JSON / stdout 一行 JSON、`need` 多轮协商）。
//! 两侧任何一处形状不一致（比如 `body` 少了一层、`ops` 名字写错），native 测试都发现不了，
//! 表现却是「面板一片空白」。这里用宿主自己的 [`PluginHost`] 把边界也测了。
//!
//! 能力用桩（[`StubCaps`]）代替真 rclone：只验证协议形状与插件行为，不碰用户数据。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use plugin_host::{Allowed, CapProvider, PluginHost};
use serde_json::{json, Value};

/// 只会应答、不会真的做事的宿主能力实现。
struct StubCaps;

#[async_trait::async_trait]
impl CapProvider for StubCaps {
    async fn rclone_call(&self, _method: String, _params: Value) -> Result<Value> {
        // `operations/list` 的回复形状：`{"result":[...]}`
        Ok(json!({ "result": [] }))
    }

    async fn upload_file(
        &self,
        _fs: String,
        _remote_path: String,
        _filename: String,
        _data_b64: String,
    ) -> Result<Value> {
        Ok(json!({ "ok": true }))
    }

    async fn kv_get_all(&self, _plugin_id: &str) -> Result<Value> {
        Ok(json!({}))
    }

    async fn kv_set(&self, _plugin_id: &str, _key: String, _value: Value) -> Result<()> {
        Ok(())
    }

    async fn kv_delete(&self, _plugin_id: &str, _key: String) -> Result<()> {
        Ok(())
    }

    async fn log(&self, _message: String) {}

    async fn cap_call(&self, op: &str, _params: Value) -> Result<Value> {
        match op {
            "fs.read_b64" => Ok(json!({ "b64": "", "bytes": 0 })),
            "rclone.cat" => Ok(json!({ "b64": "", "content_type": "image/png" })),
            "ui.copy" => Ok(json!({ "ok": true })),
            other => Ok(json!({ "ok": false, "error": rcgui_i18n::f("桩未实现 {}", &[&(other)]) })),
        }
    }
}

fn assets_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/plugins")
}

/// 遍历 `assets/plugins/` 下每个随包插件，跑 `state`，检查面板协议。
///
/// 用遍历而不是写死 id：以后加插件不用改测试，忘了写面板才会红。
#[tokio::test]
async fn shipped_plugins_answer_state_with_panel_body() {
    let root = assets_dir();
    assert!(root.is_dir(), "{}", rcgui_i18n::f("随包插件目录不存在: {}（先跑 scripts/fetch_plugins.ps1）", &[&(root.display())]));

    let mut checked = 0usize;
    let mut entries: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();

    let host = PluginHost::new().unwrap();
    let caps: Arc<dyn CapProvider> = Arc::new(StubCaps);

    for dir in entries {
        let (cfg, _report) = match plugin_host::bundle::load_from_dir(&dir, "0.1.0") {
            Ok(v) => v,
            // 有些插件包可能只放 wasm（迁移期），跳过但要说清楚
            Err(e) => {
                eprintln!("跳过 {}: {e}", dir.display());
                continue;
            }
        };
        let wasm = dir.join(&cfg.entry);
        let allowed = Allowed::from_groups(&cfg.capabilities);

        // 1) manifest 能取回来，且 id 与 config.json 一致
        let manifest = host.load_manifest(&wasm).await.unwrap();
        assert_eq!(manifest.id, cfg.id, "{}", rcgui_i18n::f("{}: manifest.id 与 config.json 的 id 不一致", &[&(cfg.id)]));

        // 2) 面板 `state` 动作回了一个可渲染的 body
        let out = host
            .invoke_with(
                &wasm,
                "request",
                json!({ "action": "state", "form": {} }),
                Some(caps.clone()),
                &cfg.id,
                &allowed,
            )
            .await
            .unwrap_or_else(|e| panic!("{}", rcgui_i18n::f("{}: state 调用失败: {}", &[&(cfg.id), &(e)])));

        let body = out
            .get("body")
            .unwrap_or_else(|| panic!("{}", rcgui_i18n::f("{}: state 没有 result.body，实际: {}", &[&(cfg.id), &(out)])));

        // 面板渲染只认这几样；至少 status 或 checks 之一要出现，否则用户看到空面板
        let has_status = body.get("status").and_then(|v| v.as_str()).is_some();
        let has_checks = body.get("checks").and_then(|v| v.as_array()).is_some();
        assert!(has_status || has_checks, "{}", rcgui_i18n::f("{}: 面板 body 既没有 status 也没有 checks: {}", &[&(cfg.id), &(body)]));

        // 3) 未知动作不许 panic，要回错误（面板可能把 error 放在 body 里，也可能给 4xx 状态）
        let bad = host
            .invoke_with(
                &wasm,
                "request",
                json!({ "action": "__no_such_action__", "form": {} }),
                Some(caps.clone()),
                &cfg.id,
                &allowed,
            )
            .await;
        match bad {
            Ok(v) => {
                // 三种「拒绝」写法宿主都能显示：result.error、4xx 状态、或面板红灯 + 文案
                let status_bad = v.get("status").and_then(|s| s.as_u64()).is_some_and(|s| s >= 400);
                let bad_lamp = v
                    .pointer("/body/lamp")
                    .and_then(|l| l.as_str())
                    .is_some_and(|l| l == "err");
                let text = v.to_string();
                assert!(status_bad || bad_lamp || text.contains("\"error\""), "{}", rcgui_i18n::f("{}: 未知动作应当明确报错，实际: {}", &[&(cfg.id), &(v)]));
            }
            Err(_) => { /* 宿主层报错也算拒绝，可接受 */ }
        }

        checked += 1;
    }

    assert!(checked > 0, "{}", rcgui_i18n::t("随包插件目录里一个可加载的插件都没有"));
}
