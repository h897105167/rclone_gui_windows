//! 让真的 K-Vault 插件 wasm 跑在真的宿主桥上。
//!
//! `crates/app` 里的测试替身是假插件，证明不了双方接线对不对；这里跑的是发布出去
//! 的那个 `.wasm`，经 `plugin_host::invoke_with`，用的是主程序调的同一个入口。任何
//! 一侧改了协议，红的是这里的测试，而不是用户的文件浏览页。
//!
//! 插件在另一个仓库，所以包是输入而不是本仓库的静态夹具：
//! `RCGUI_IT_WASM` 指向 `main.wasm`，`RCGUI_IT_CONFIG` 指向它的 `config.json`
//! （缺省取 wasm 旁边那个）。两个路径都应当写成**绝对路径**：cargo 跑测试时把工作
//! 目录设成这个包的目录（`crates/plugin-it`），相对路径会指到别处去，而"文件不存在"
//! 和"没设变量"看起来一模一样。
//!
//! 两种模式：
//! - 默认（本机）：变量没设 → 每个测试打印 [`SKIP_NOTE`] 后返回，`cargo test --workspace`
//!   在没有插件的机器上仍然是绿的。
//! - `RCGUI_IT_REQUIRED=1`（CI 用）：变量没设、路径不存在、文件是空的 → 直接失败。
//!   否则 CI 可以在一个测试都没跑的情况下变绿，这正是要防的。

pub mod host;
pub mod site;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use plugin_host::config::PluginConfig;
use plugin_host::{Allowed, CapProvider, PluginHost};
use serde_json::{json, Value};

pub use host::MockHost;
pub use site::MockSite;

/// 宿主把请求路由到哪个插件 id。这是插件自己 `config.json` 里的 id；写错的话
/// 表现是它的设置命名空间读出来是空的。
pub const PLUGIN_ID: &str = "kvault";

/// 没设环境变量时打在屏幕上的话。
pub const SKIP_NOTE: &str =
    "RCGUI_IT_WASM 未设置：跳过跨仓库测试（设为插件的 main.wasm 路径即可运行）";

/// 设成 `1` 就把「跳过」变成「失败」。CI 必须设，本机默认不设。
pub const REQUIRED_ENV: &str = "RCGUI_IT_REQUIRED";

/// 这个包目录，用来在报错里说明相对路径被解析到了哪里。
fn here() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".to_string())
}

/// `RCGUI_IT_REQUIRED` 非空且不是 0/false/no 时为真。
fn required() -> bool {
    match std::env::var(REQUIRED_ENV) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no"
        ),
        Err(_) => false,
    }
}

/// 拿不到输入时的统一出口：CI 里直接失败，本机只打印一句。
///
/// 返回类型是泛型的，因为调用方有两种：`Fixture::load()` 要 `Option<Fixture>`，
/// `Harness::start()` 要 `Option<Harness>`。`panic!` 在这里返回 `!`，两种都对得上。
fn absent<T>(why: String) -> Option<T> {
    if required() {
        panic!(
            "{why}\n当前工作目录是 {cwd}（cargo 跑测试时把工作目录设成包目录，\
             所以环境变量里的相对路径会指到这里）。{REQUIRED_ENV} 已设置，这种情况必须失败，\
             不能当成「跳过」蒙过去。",
            cwd = here(),
        );
    }
    eprintln!("{why}\n{SKIP_NOTE}");
    None
}

/// 被测插件的所在位置。
#[derive(Clone, Debug)]
pub struct Fixture {
    pub wasm: PathBuf,
    pub config: PathBuf,
}

impl Fixture {
    /// 返回 `None` 表示应当跳过，而不是失败；设了 [`REQUIRED_ENV`] 就改成 panic。
    pub fn load() -> Option<Fixture> {
        let raw = match std::env::var("RCGUI_IT_WASM") {
            Ok(v) => v,
            Err(_) => return absent("RCGUI_IT_WASM 没设。".to_string()),
        };
        let wasm = PathBuf::from(&raw);
        if !wasm.is_file() {
            return absent(format!(
                "RCGUI_IT_WASM 指向的文件不存在: {raw}（绝对路径：{}）",
                wasm.display()
            ));
        }
        // 空文件（clone 失败、编到一半中断）比不存在更难查：`is_file()` 是 true，
        // 后面的 manifest 调用会报一句看不懂的 wasm 解析错误。
        match std::fs::metadata(&wasm) {
            Ok(m) if m.len() > 0 => {}
            _ => return absent(format!("RCGUI_IT_WASM 指向的是空文件: {}", wasm.display())),
        }
        let config = match std::env::var("RCGUI_IT_CONFIG") {
            Ok(p) => PathBuf::from(p),
            Err(_) => wasm.with_file_name("config.json"),
        };
        if !config.is_file() {
            return absent(format!(
                "找不到插件 config.json: {}（RCGUI_IT_CONFIG 没设时取 wasm 旁边那个）",
                config.display()
            ));
        }
        Some(Fixture { wasm, config })
    }

    /// 插件自己声明的能力。**不在这里另抄一份**：插件哪天不声明 `http` 了，应当是
    /// 这里红，而不是上线之后才发现。
    pub fn allowed(&self) -> Result<Allowed> {
        let raw = std::fs::read(&self.config)?;
        let cfg: PluginConfig = serde_json::from_slice(&raw)?;
        Ok(Allowed::from_groups(&cfg.capabilities))
    }

    pub fn config_json(&self) -> Result<PluginConfig> {
        Ok(serde_json::from_slice(&std::fs::read(&self.config)?)?)
    }

    /// 上一版安装留下的那份 `config.json`：版本号改成 `version`，其余字节照旧。
    ///
    /// 真实升级里这一幕很常见：wasm 换了，插件目录里的 config 还是旧版本号（手动换文件、
    /// 或者只更新了半个包）。宿主只按 `capabilities` 决定放不放行，从不比对版本号，
    /// 所以「config 说旧、wasm 是新」必须能正常用。造在临时目录里，不动被测包里的原件。
    ///
    /// 读不出来或写不进去时返回 `None`——由调用方按「跳过还是失败」处理。
    pub fn with_version(&self, version: &str) -> Option<Fixture> {
        let raw = std::fs::read(&self.config).ok()?;
        let mut cfg: Value = serde_json::from_slice(&raw).ok()?;
        cfg["version"] = json!(version);
        let dir = std::env::temp_dir().join(format!("plugin-it-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("config-{version}.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(&cfg).ok()?).ok()?;
        Some(Fixture {
            wasm: self.wasm.clone(),
            config: path,
        })
    }
}

/// 一个假站点 + 宿主桥，随时可以回答插件。
pub struct Harness {
    pub site: MockSite,
    pub host: Arc<MockHost>,
    pub plugin: Arc<PluginHost>,
    pub fixture: Fixture,
    pub allowed: Allowed,
}

impl Harness {
    /// 插件不可用（没设变量、文件不在）时返回 `None`，与 `Fixture::load` 一致；
    /// 设了 [`REQUIRED_ENV`] 就改成 panic——包括配置读不动、运行时起不来这两种。
    ///
    /// 站点由调用方先起好再传进来，因为宿主的 `config/get` 回答里要写站点地址。
    pub async fn start(site: MockSite, host: Arc<MockHost>) -> Option<Harness> {
        let fixture = Fixture::load()?;
        Harness::with_fixture(fixture, site, host).await
    }

    /// 同 [`Harness::start`]，但夹具由调用方给：升级用例要拿一份「上一版的 config.json」，
    /// 那份文件不在环境变量指的目录里。
    pub async fn with_fixture(
        fixture: Fixture,
        site: MockSite,
        host: Arc<MockHost>,
    ) -> Option<Harness> {
        let allowed = match fixture.allowed() {
            Ok(a) => a,
            Err(e) => return absent(format!("插件 config.json 读不出来: {e:#}")),
        };
        let plugin = match PluginHost::new() {
            Ok(p) => Arc::new(p),
            Err(e) => return absent(format!("宿主插件运行时起不来: {e:#}")),
        };
        Some(Harness {
            site,
            host,
            plugin,
            fixture,
            allowed,
        })
    }

    /// 调一个插件动作，返回插件的**业务返回**（`result.body`），不是整份 `result`。
    ///
    /// 主程序的 `plugins::call_plugin` 就是这么取的：上层要的是「列表有几条」「链接是什么」，
    /// 不是 `{status, body}` 这个信封。测试照同一层取，断言才和真实行为对得上——
    /// 取错层的话 `items` 永远是空的，测试会红在一个看不出原因的地方。
    pub async fn call(&self, action: &str, extra: Value) -> Result<Value> {
        self.call_allowed(&self.allowed, action, extra).await
    }

    /// 同上，但能力集合由测试指定——用来证明「没声明就该拒绝」，而不是放行。
    pub async fn call_allowed(
        &self,
        allowed: &Allowed,
        action: &str,
        extra: Value,
    ) -> Result<Value> {
        // 表单留空：`remote.*` 读的是顶层参数（`remote`/`path`/`items`…），不读表单；
        // 主程序那里的表单是面板默认值合并出来的，测试里没有面板。
        // `lang` 也不传：`invoke_with` 每轮都会用宿主当前语言覆盖这个键。
        let mut params = json!({
            "action": action,
            "form": {},
            "now": now_json(),
        });
        if let (Some(dst), Some(src)) = (params.as_object_mut(), extra.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        let caps = Some(self.host.clone() as Arc<dyn CapProvider>);
        let out = self
            .plugin
            .invoke_with(
                &self.fixture.wasm,
                "request",
                params,
                caps,
                PLUGIN_ID,
                allowed,
            )
            .await?;
        // 缺 `body` 时按空对象算，与主程序一致；插件用 `fail()` 报的业务失败也在 body 里
        // （`{status, lamp, toast}`），所以失败信息同样读这一层。
        Ok(out.get("body").cloned().unwrap_or(Value::Null))
    }

    /// 问 wasm 自己的清单，用来证明这个二进制与旁边的 config 是同一版。
    pub async fn manifest(&self) -> Result<Value> {
        self.plugin
            .load_manifest(&self.fixture.wasm)
            .await
            .map(|m| json!({ "id": m.id, "name": m.name, "version": m.version }))
    }
}

/// 与主程序 `plugins::now_json()` 一模一样的那份「现在」。
///
/// 键少一个，插件里按 `now.date` 造默认目录的地方就会走另一条分支，
/// 测试也就验不到真实行为。这里刻意照抄，不做精简。
fn now_json() -> Value {
    let now = chrono::Local::now();
    json!({
        "unix": now.timestamp(),
        "unix_ms": now.timestamp_millis(),
        "local": now.format("%Y-%m-%dT%H:%M:%S").to_string(),
        "date": now.format("%Y-%m-%d").to_string(),
        "time": now.format("%H:%M:%S").to_string(),
        "tz_offset_min": now.offset().local_minus_utc() / 60,
    })
}

/// 断言应当失败，且失败原因里含给定片段之一。
///
/// 返回错误全文，测试可以再对细节下断言。
pub fn expect_error(res: Result<Value>, fragments: &[&str]) -> String {
    match res {
        Ok(v) => panic!("本以为会失败，却成功了: {v}"),
        Err(e) => {
            let text = format!("{e:#}");
            for f in fragments {
                if text.contains(f) {
                    return text;
                }
            }
            panic!("错误信息里没有 {:?}: {text}", fragments);
        }
    }
}

/// 一条能用的 K-Vault 远程：假站点已经起好，宿主的回答指向它。
pub async fn kvault_harness() -> Option<Harness> {
    let site = MockSite::start();
    let host = MockHost::kvault(&site.dav_url(), "test-token");
    Harness::start(site, host).await
}

/// 同上，但那条远程是普通 WebDAV，没有任何标记参数。
pub async fn plain_webdav_harness() -> Option<Harness> {
    let site = MockSite::start();
    let host = MockHost::plain_webdav(&site.dav_url(), "alice");
    Harness::start(site, host).await
}
