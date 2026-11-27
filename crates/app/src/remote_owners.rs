//! 哪些远程归哪个插件管：一张明确的、写进数据目录的登记表。
//!
//! 归属不靠猜。一条远程只有在**被明确登记过**之后才算插件管的，登记只发生在三处：
//!
//! - 用户用插件提供的类型新建或保存了一条远程（保存成功后按 rclone 里实际存下的
//!   参数登记）；
//! - 远程配置被整体导入（每条写成功就登记）；
//! - 升级后第一次启动，按 [`LEGACY`] 那张历史表把旧配置扫一遍。
//!
//! 登记表放在应用数据目录（`remote_owners.json`），**不在插件目录里**：插件被停用、
//! 卸载、插件目录被删，这张表都还在。于是「这条远程是插件管的，而插件现在不在」
//! 仍然认得出，主程序会拒绝操作，而不是换一种协议去读写用户的数据。
//!
//! ## 为什么不再看参数猜
//!
//! 远程参数里除了 `type` 没有「谁的标记」这种约定。S3 有 `bucket`/`region`/
//! `endpoint`，Drive 有 `root_folder_id`，用户自己也会加字段，WebDAV 还有
//! `bearer_token`。把「值为真的陌生键」当成插件标记，等于把普通远程随机判成
//! 插件远程，打不开还算轻的——真去拒绝操作会让人以为配置坏了。
//!
//! ## 读不出来就是读不出来
//!
//! 登记表读失败（权限、损坏、被占用）时这里返回错误，调用方必须**拒绝**依赖归属
//! 的操作并说明原因。把它当成「没有登记」等于悄悄退回原生访问，而原生协议对插件
//! 远程根本不对。只有一种情况可以返回「没有登记」：文件不存在，或者本次启动的
//! 历史扫描确实跑完了。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rcgui_profile::bundle::{RcguiConf, RemoteOwnerRef};

use crate::config;
use crate::remote_types::{self, Derived};
use crate::Ctx;

/// 登记表文件名，位于 [`config::data_dir`]。
pub const FILE: &str = "remote_owners.json";

/// 自己在文件里写的格式版本。读到更大的版本只警告不破坏：多出来的字段反序列化时
/// 会被忽略，写回时也就丢了——所以只在本进程确实要改某一条时才重写整张表。
const VERSION: u64 = 1;

/// 历史上由主程序内置实现、现在已经整体搬进插件的远程类型。
///
/// 三项依次是：远程参数里的标记键、负责这个类型的插件 id、给用户看的名字。
///
/// 这张表只用于「认出旧配置并登记」，登记完就按登记表办事：插件不在时操作被拒绝，
/// 主程序不会拿它代替插件干活，也不会据此解析任何远端内容，更不会因为这张表去删
/// 用户的数据。
pub const LEGACY: &[(&str, &str, &str)] = &[("kvault", "kvault", "K-Vault")];

/// 一条远程的归属记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// 认领这条远程的插件 id。
    pub plugin: String,
    /// 这条远程参数里的标记键，也就是插件清单里声明的那一个。
    pub marker: String,
    /// 登记时插件的显示名。插件不在时用它把话说清楚。
    pub label: String,
    /// 谁登记的：`declared`（插件声明的标记）、`legacy`（历史表）、`scan`（启动扫描）。
    pub source: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Table {
    #[serde(default)]
    version: u64,
    #[serde(default)]
    remotes: BTreeMap<String, Claim>,
}

/// 登记表的完整路径。
pub fn path() -> PathBuf {
    config::data_dir().join(FILE)
}

/// 从指定文件读登记表。文件不存在算空表；存在但读不出来或看不懂是错误。
pub fn load_from(file: &Path) -> Result<BTreeMap<String, Claim>, String> {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("{} 读不出来：{}", file.display(), e)),
    };
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(BTreeMap::new());
    }
    let table: Table = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} 内容看不懂：{}", file.display(), e))?;
    Ok(table.remotes)
}

/// 把登记表写到指定文件。
pub fn save_to(file: &PathBuf, remotes: &BTreeMap<String, Claim>) -> Result<(), String> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录 {}：{}", dir.display(), e))?;
    }
    let table = Table {
        version: VERSION,
        remotes: remotes.clone(),
    };
    let text = serde_json::to_string_pretty(&table).map_err(|e| format!("序列化登记表：{e}"))?;
    config::write_atomic(file, text.as_bytes()).map_err(|e| format!("写 {}：{}", file.display(), e))
}

/// 读当前数据目录里的登记表。
pub fn load() -> Result<BTreeMap<String, Claim>, String> {
    load_from(&path())
}

/// 登记这条远程归某个插件管。值没变就不写盘（保存远程时会走到这里）。
pub fn claim(remote: &str, c: Claim) -> Result<(), String> {
    let name = remote.trim();
    if name.is_empty() {
        return Ok(());
    }
    let mut table = load()?;
    if table.get(name) == Some(&c) {
        return Ok(());
    }
    table.insert(name.to_string(), c);
    let file = path();
    save_to(&file, &table)
}

/// 这条远程不再归任何插件管（用户删了它）。插件替它存的历史设置由调用方另行处理，
/// 这里只动登记表。
pub fn release(remote: &str) -> Result<(), String> {
    let name = remote.trim();
    let mut table = load()?;
    if table.remove(name).is_none() {
        return Ok(());
    }
    let file = path();
    save_to(&file, &table)
}

/// 本次启动的历史扫描失败原因。存在期间，「查不到登记」不等于「没有登记」。
static SCAN_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// 这次启动**读不回配置**的远程：名字 → 原因。`None` 表示还没扫过。
///
/// 只影响被点名的这几条，其余远程照常判断。一条远程连配置都读不出来时，说不清它
/// 归谁，[`resolve`] 对它返回错误，而不是「不属于插件」。
static UNREADABLE: Mutex<Option<BTreeMap<String, String>>> = Mutex::new(None);

/// 这次启动里**当场读过一次、并且读成功**的远程。
///
/// 启动扫描可能读不到某条远程的配置（内核刚起来就断了、那一刻正好超时），
/// 也可能整次扫描都没跑成。这两种情况都不该让一条普通远程锁到下次启动：
/// 用户真要打开它时，宿主会就这一条再读一次配置，读成功就按实际配置重新判一次
/// （见 [`note_remote`]），并把名字记在这里 —— 记下之后 [`decide`] 就不再拿
/// 「扫描失败」去挡它，因为这条已经没有未知项了。
static EXAMINED: Mutex<Option<BTreeSet<String>>> = Mutex::new(None);

/// 记下（或清掉）这次启动的历史扫描结果。
pub fn note_scan_error(msg: Option<String>) {
    if let Ok(mut g) = SCAN_ERROR.lock() {
        *g = msg;
    }
}

/// 这次启动的历史扫描失败原因，没有就是 `None`。
pub fn scan_error() -> Option<String> {
    SCAN_ERROR.lock().ok().and_then(|g| g.clone())
}

/// 记下这次启动读不回配置的远程（整份替换）。空表表示「扫过了，都能读」。
pub fn note_unreadable(names: BTreeMap<String, String>) {
    if let Ok(mut g) = UNREADABLE.lock() {
        *g = Some(names);
    }
}

/// 记下「这条远程这次已经当场读过配置、读成功了」。
fn note_examined(name: &str) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    if let Ok(mut g) = EXAMINED.lock() {
        let set = g.get_or_insert_with(BTreeSet::new);
        set.insert(name.to_string());
    }
    // 读成功了就不再算「这次读不回来」，否则它会被两条相反的记录同时描述。
    if let Ok(mut g) = UNREADABLE.lock() {
        if let Some(m) = g.as_mut() {
            m.remove(name);
        }
    }
}

/// 这次已经当场读过、读成功的远程名单（`decide` 用）。
fn examined_set() -> BTreeSet<String> {
    EXAMINED
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
}

/// 归属判断的纯逻辑部分：给一份登记表、本次已经当场读成功过的远程、已知读不回的名单、
/// 扫描错误，回答一条远程归谁。
///
/// 抽出来是为了能直接测「读不回来时必须拒绝」——`resolve` 自己要读数据目录里的文件。
pub fn decide(
    table: &BTreeMap<String, Claim>,
    examined: &BTreeSet<String>,
    unreadable: &BTreeMap<String, String>,
    scan_error: Option<&str>,
    remote: &str,
) -> Result<Option<Claim>, String> {
    let name = remote.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if let Some(c) = table.get(name) {
        return Ok(Some(c.clone()));
    }
    // 这一条这次已经当场读过配置、读成功了：它归不归插件早有答案（没有登记就是没有），
    // 不必再拿整次扫描的失败去挡它。
    if examined.contains(name) {
        return Ok(None);
    }
    // 没登记：只有「文件读成功 + 扫描跑完 + 这条远程的配置读得回来」时才敢说它不归插件。
    if let Some(e) = scan_error {
        return Err(e.to_string());
    }
    match unreadable.get(name) {
        Some(e) => Err(e.clone()),
        None => Ok(None),
    }
}

/// 查一条远程的归属。
///
/// `Ok(None)` 只代表「它不属于任何插件」，而且这个判断是可信的（文件读成功、
/// 启动扫描没失败、它的配置读得回来）。读表失败、扫描失败、或这条远程的配置读不回来时
/// 返回 `Err`，调用方应当拒绝操作并把原因告诉用户。
pub fn resolve(remote: &str) -> Result<Option<Claim>, String> {
    let table = load()?;
    let unreadable = UNREADABLE
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default();
    let scan_err = scan_error();
    decide(
        &table,
        &examined_set(),
        &unreadable,
        scan_err.as_deref(),
        remote,
    )
}

/// 按一份参数判断它是不是插件类型建出来的，是就给出该登记什么。
///
/// 先看已启用插件声明的标记（`declared`），再看 [`LEGACY`] 的历史标记（`legacy`）。
/// 两步都是**按名单点名**，没有任何「陌生键 ⇒ 插件远程」的推断。
pub fn claim_of(table: &[Derived], params: &Map<String, Value>) -> Option<Claim> {
    if let Some(d) = remote_types::owner_of_params(table, params) {
        return Some(Claim {
            plugin: d.plugin.clone(),
            marker: d.marker.clone(),
            label: d.name.clone(),
            source: "declared".to_string(),
        });
    }
    for (marker, plugin, label) in LEGACY {
        if remote_types::marker_is_set(params.get(*marker)) {
            return Some(Claim {
                plugin: (*plugin).to_string(),
                marker: (*marker).to_string(),
                label: (*label).to_string(),
                source: "legacy".to_string(),
            });
        }
    }
    None
}

/// 保存一条远程之后登记归属，以 rclone 里**实际存下的参数**为准。
///
/// 入参那份（界面上的表单 + 插件默认值）不一定等于最终结果：rclone 会补自己的默认
/// 值、会去掉空键。所以登记前重新 `config/get` 一次。读不回来就是错误——这时候
/// 说不清这条远程归谁，不能凭界面猜。
pub async fn note_remote(ctx: &Arc<Ctx>, remote: &str) -> Result<(), String> {
    let name = remote.trim();
    if name.is_empty() {
        return Ok(());
    }
    let client = ctx.rc.client().await;
    let params = client
        .config_get(name)
        .await
        .map_err(|e| format!("读不回 {} 存好的配置：{}", name, e))?;
    // 这一条这次已经读过、读成功了：后面无论如何判断，都不再算「有未知项」。
    note_examined(name);
    match claim_of(&remote_types::collect(ctx), &params) {
        Some(c) => claim(name, c),
        // 用户把插件类型改成了普通类型：旧登记必须撤掉，否则这条远程会一直被
        // 当成「插件不在」而打不开。
        None => release(name),
    }
}

/// 启动时扫一遍旧配置，把历史插件远程登记进表。
///
/// 只登记**还没有登记**的远程（已登记的不重算，免得插件停用期间把它们抹掉）。
/// 读某个远程失败时记下它的名字：说不清它归谁，[`resolve`] 会对它返回错误；整次
/// 扫描失败（读不出远程列表、登记表坏了）则记进 [`note_scan_error`]，那种情况下
/// 任何「查不到登记」都不可信。
pub async fn scan_all(ctx: &Arc<Ctx>) -> Vec<String> {
    note_unreadable(BTreeMap::new());
    let client = ctx.rc.client().await;
    let names = match client.list_remotes().await {
        Ok(n) => n,
        Err(e) => {
            let msg = rcgui_i18n::f(
                "这次启动没能读出远程列表，插件远程的归属无从判断：{}",
                &[&(e)],
            );
            note_scan_error(Some(msg.clone()));
            return vec![msg];
        }
    };
    let table = remote_types::collect(ctx);
    let mut known = match load() {
        Ok(t) => t,
        Err(e) => {
            let msg = rcgui_i18n::f(
                "远程归属登记表读不出来，插件远程的操作会全部被拒绝：{}",
                &[&(e)],
            );
            note_scan_error(Some(msg.clone()));
            return vec![msg];
        }
    };
    let mut notes: Vec<String> = Vec::new();
    let mut changed = false;
    let mut unreadable: BTreeMap<String, String> = BTreeMap::new();
    for name in names {
        if known.contains_key(&name) {
            continue;
        }
        let params = match client.config_get(&name).await {
            Ok(p) => p,
            Err(e) => {
                unreadable.insert(
                    name.clone(),
                    format!(
                        "远程 {name} 的配置这次读不出来，说不清它归谁，所以先不给它下判断：{e}"
                    ),
                );
                continue;
            }
        };
        if let Some(mut c) = claim_of(&table, &params) {
            c.source = "scan".to_string();
            known.insert(name.clone(), c);
            changed = true;
        }
    }
    if changed {
        let file = path();
        if let Err(e) = save_to(&file, &known) {
            note_scan_error(Some(e.clone()));
            note_unreadable(unreadable);
            notes.push(e);
            return notes;
        }
    }
    note_scan_error(None);
    note_unreadable(unreadable);
    notes
}

/// 导出时把登记表变成配置文件里的那一段。
///
/// 只带「谁归谁」，不带任何远端内容，也不带凭据。
pub fn export_refs(table: &BTreeMap<String, Claim>) -> Vec<RemoteOwnerRef> {
    table
        .iter()
        .map(|(remote, c)| RemoteOwnerRef {
            remote: remote.clone(),
            plugin: c.plugin.clone(),
            marker: c.marker.clone(),
            label: c.label.clone(),
            source: c.source.clone(),
        })
        .collect()
}

/// 导入一条归属记录时的结论。
pub enum ImportDecision {
    /// 可以登记；里面的插件名与标记以本机实际为准。
    Claim(Claim),
    /// 不登记，附上要告诉用户的原因。
    Skip(String),
}

/// 判断文件里的一条归属记录能不能在本机落地。
///
/// 判据是本机远程的**实际参数**，不是文件怎么说：文件可能来自另一台机器、可能是插件
/// 卸掉之前导出的，甚至可能是用户手动改过的。只有参数里那个标记键确实是真值，才认为
/// 这台远程真的归那个插件；否则不登记并说明，绝不因为文件里写了就锁住一台普通远程。
pub fn decide_import(
    rec: &RemoteOwnerRef,
    live: Option<&Map<String, Value>>,
    declared: &[Derived],
) -> ImportDecision {
    let name = rec.remote.trim();
    let plugin = rec.plugin.trim();
    let marker = rec.marker.trim();
    if name.is_empty() || plugin.is_empty() || marker.is_empty() {
        return ImportDecision::Skip(
            rcgui_i18n::t("文件里有一条归属记录缺远程名、插件 id 或标记键，跳过这一条").to_string(),
        );
    }
    let Some(params) = live else {
        return ImportDecision::Skip(rcgui_i18n::f(
            "本机没有远程「{}」，文件里那条归属记录没有登记",
            &[&(name)],
        ));
    };
    if !remote_types::marker_is_set(params.get(marker)) {
        return ImportDecision::Skip(rcgui_i18n::f(
            "本机远程「{}」的参数里没有标记键「{}」，它现在是普通远程，文件里那条归属记录没有登记",
            &[&(name), &(marker)],
        ));
    }
    // 插件在场时以它现在声明的东西为准：文件可能来自旧版本，插件名、标记键都可能变过。
    if let Some(d) = declared.iter().find(|d| d.marker == marker) {
        return ImportDecision::Claim(Claim {
            plugin: d.plugin.clone(),
            marker: d.marker.clone(),
            label: d.name.clone(),
            source: "import".to_string(),
        });
    }
    // 插件不在（卸载之后重装、或这台机器还没装）：文件里的记录就是唯一线索。登记它
    // 意味着插件回来之前这台远程会被拒绝，而不是退回原生协议去读写。
    let label = if rec.label.trim().is_empty() {
        plugin.to_string()
    } else {
        rec.label.trim().to_string()
    };
    ImportDecision::Claim(Claim {
        plugin: plugin.to_string(),
        marker: marker.to_string(),
        label,
        source: "import".to_string(),
    })
}

/// 合并一份配置文件带回来的归属记录，得到要写回去的整张表。
pub struct Merged {
    /// 合并后的表（本机原有的登记都在里面）。
    pub table: BTreeMap<String, Claim>,
    /// 新登记或改登记的条数。
    pub claimed: usize,
    /// 因本机参数已无标记而撤销的条数。
    pub dropped: usize,
    /// 要告诉用户的话。
    pub notes: Vec<String>,
}

/// 把文件里的归属记录并进本机的表。
///
/// 三条规则：
///
/// - 只增不删本机**没有提到**的远程：别的插件、别的账户的登记原样留着。
/// - 文件里列了某台远程却没带它的归属记录时，以本机参数为准：参数里那个标记已经
///   没了（用户把它改成了普通远程）就撤掉旧登记，否则这条远程会一直打不开。
/// - 参数读不出来时什么都不动，并说一句：宁可不放开，也不要有一次「悄悄退回原生」。
pub fn merge_imported(
    imported: &[RemoteOwnerRef],
    package_remotes: &BTreeSet<String>,
    live: &BTreeMap<String, Option<Map<String, Value>>>,
    declared: &[Derived],
    current: &BTreeMap<String, Claim>,
) -> Merged {
    let mut table = current.clone();
    let mut claimed = 0usize;
    let mut dropped = 0usize;
    let mut notes: Vec<String> = Vec::new();
    for rec in imported {
        let name = rec.remote.trim().to_string();
        let params = live.get(&name).and_then(|p| p.as_ref());
        match decide_import(rec, params, declared) {
            ImportDecision::Skip(why) => notes.push(why),
            ImportDecision::Claim(c) => {
                let old = table.get(&name).cloned();
                if old.as_ref() == Some(&c) {
                    continue;
                }
                if let Some(old) = old {
                    if old.plugin != c.plugin {
                        notes.push(rcgui_i18n::f(
                            "远程「{}」原本登记给插件「{}」，这份文件说是「{}」，本机参数与文件一致，已改成后者",
                            &[&(name), &(old.label), &(c.label)],
                        ));
                    }
                }
                table.insert(name, c);
                claimed += 1;
            }
        }
    }
    for name in package_remotes {
        if imported.iter().any(|r| r.remote.trim() == name.as_str()) {
            continue;
        }
        let Some(claim) = table.get(name).cloned() else {
            continue;
        };
        match live.get(name).and_then(|p| p.as_ref()) {
            None => notes.push(rcgui_i18n::f(
                "远程「{}」还登记为插件「{}」管的，但这次没读到它的参数，这条登记没有动",
                &[&(name), &(claim.label)],
            )),
            Some(params) => {
                if !remote_types::marker_is_set(params.get(claim.marker.trim())) {
                    table.remove(name);
                    dropped += 1;
                    notes.push(rcgui_i18n::f(
                        "远程「{}」在文件里是普通远程、本机参数里也没有「{}」标记，原来的登记撤掉了",
                        &[&(name), &(claim.marker)],
                    ));
                }
            }
        }
    }
    Merged {
        table,
        claimed,
        dropped,
        notes,
    }
}

/// 写回登记表：先留一份 `.bak-时间戳`，写失败就把备份放回去。
///
/// 返回备份路径（表原本不存在时为 `None`）。写失败必须把原因交出去——调用方不能当它
/// 成功，否则后面的操作会以为归属已经记好了。
pub fn save_with_backup(
    file: &PathBuf,
    remotes: &BTreeMap<String, Claim>,
) -> Result<Option<PathBuf>, String> {
    let bak = if file.exists() {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| FILE.to_string());
        let bak = file.with_file_name(format!("{name}.bak-{stamp}"));
        std::fs::copy(file, &bak).map_err(|e| format!("备份登记表到 {}：{}", bak.display(), e))?;
        Some(bak)
    } else {
        None
    };
    if let Err(e) = save_to(file, remotes) {
        if let Some(b) = bak.as_ref() {
            let _ = std::fs::copy(b, file);
        }
        return Err(e);
    }
    Ok(bak)
}

/// 按导入的配置文件重算归属，并把结果写回登记表。
///
/// 只动这份文件提到的远程，本机别处的登记原样留着。登记表读不出来时**一个字节都不
/// 写**：那种时候连原表长什么样都不清楚，往上盖会把用户的归属记录弄丢。
pub async fn adopt_from_bundle(ctx: &Arc<Ctx>, conf: &RcguiConf) -> Vec<String> {
    if conf.remote_owners.is_empty() && conf.remotes.is_empty() {
        return Vec::new();
    }
    let current = match load() {
        Ok(t) => t,
        Err(e) => {
            return vec![rcgui_i18n::f(
                "远程归属登记表读不出来，这次导入没有动它：{}",
                &[&(e)],
            )]
        }
    };
    let client = ctx.rc.client().await;
    let mut live: BTreeMap<String, Option<Map<String, Value>>> = BTreeMap::new();
    let mut package_remotes: BTreeSet<String> = BTreeSet::new();
    for r in &conf.remotes {
        let name = r.name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        package_remotes.insert(name.clone());
        if live.contains_key(&name) {
            continue;
        }
        let params = client.config_get(&name).await.ok();
        live.insert(name, params);
    }
    let declared = remote_types::collect(ctx);
    let merged = merge_imported(
        &conf.remote_owners,
        &package_remotes,
        &live,
        &declared,
        &current,
    );
    let mut notes = merged.notes;
    if merged.table != current {
        let file = path();
        match save_with_backup(&file, &merged.table) {
            Ok(Some(bak)) => notes.push(rcgui_i18n::f(
                "远程归属登记已更新（新登记 {} 条，撤销 {} 条），改动前的表存为 {}",
                &[
                    &(merged.claimed),
                    &(merged.dropped),
                    &(bak.display().to_string()),
                ],
            )),
            Ok(None) => notes.push(rcgui_i18n::f(
                "远程归属登记已建立（新登记 {} 条）",
                &[&(merged.claimed)],
            )),
            Err(e) => notes.push(rcgui_i18n::f(
                "远程归属登记没写成功，导入的其他部分已完成：{}",
                &[&(e)],
            )),
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rcgui-owners-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn params(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    fn derived(id: &str, marker: &str) -> Derived {
        Derived {
            id: id.to_string(),
            name: id.to_string(),
            base_type: "webdav".to_string(),
            defaults: Default::default(),
            plugin: id.to_string(),
            marker: marker.to_string(),
            ops: Vec::new(),
            legacy: Vec::new(),
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_table_not_an_error() {
        let dir = temp_dir("missing");
        let got = load_from(&dir.join(FILE)).expect("没有文件时是空表");
        assert!(got.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_is_an_error_not_an_empty_table() {
        let dir = temp_dir("broken");
        let file = dir.join(FILE);
        std::fs::write(&file, b"{ this is not json").expect("write");
        let got = load_from(&file);
        // 读坏了必须报错：当成空表等于「所有插件远程都变回原生远程」。
        assert!(got.is_err(), "坏文件不能被当成空表：{got:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_claim_round_trips_and_survives_without_the_plugin() {
        let dir = temp_dir("roundtrip");
        let file = dir.join(FILE);
        let mut table: BTreeMap<String, Claim> = BTreeMap::new();
        table.insert(
            "站点相册".to_string(),
            Claim {
                plugin: "kvault".to_string(),
                marker: "kvault".to_string(),
                label: "K-Vault".to_string(),
                source: "declared".to_string(),
            },
        );
        save_to(&file, &table).expect("save");
        let back = load_from(&file).expect("load");
        assert_eq!(back.get("站点相册"), table.get("站点相册"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_legacy_marker_is_claimed_by_its_plugin() {
        let c = claim_of(
            &[],
            &params(json!({"type": "webdav", "url": "https://x/dav", "kvault": "1"})),
        )
        .expect("历史标记键应当被认出来");
        assert_eq!(c.plugin, "kvault");
        assert_eq!(c.marker, "kvault");
        assert_eq!(c.source, "legacy");
    }

    #[test]
    fn an_off_legacy_marker_is_not_a_claim() {
        for v in [
            json!("0"),
            json!(""),
            json!(false),
            json!("false"),
            json!("no"),
        ] {
            let p = params(json!({ "type": "webdav", "kvault": v }));
            assert!(claim_of(&[], &p).is_none(), "关闭值不算登记：{p:?}");
        }
    }

    #[test]
    fn other_backends_and_custom_keys_are_never_claimed() {
        // S3、Drive、SFTP 与用户自造字段都是普通远程。参数里出现陌生键不等于
        // 「这是某个插件的远程」——这正是上一版按陌生键猜归属的错处。
        let cases = [
            json!({"type": "s3", "provider": "Other", "bucket": "photos", "region": "us-east-1", "endpoint": "https://s3.example", "access_key_id": "AKIA"}),
            json!({"type": "drive", "root_folder_id": "0ABC", "scope": "drive", "bucket": "x"}),
            json!({"type": "sftp", "host": "h", "user": "u", "region": "r"}),
            json!({"type": "webdav", "url": "https://x/dav", "vendor": "other", "bucket": "b", "endpoint": "e"}),
        ];
        for c in cases {
            let p = params(c.clone());
            assert!(claim_of(&[], &p).is_none(), "普通远程被误判成插件远程：{c}");
        }
    }

    #[test]
    fn a_declared_marker_is_preferred_over_the_legacy_table() {
        let table = [derived("kvault", "kvault")];
        let c = claim_of(
            &table,
            &params(json!({"type": "webdav", "kvault": "1", "kvault_folder": "相册"})),
        )
        .expect("插件在场时按声明登记");
        assert_eq!(c.source, "declared");
        assert_eq!(c.plugin, "kvault");
    }

    #[test]
    fn an_unregistered_remote_is_native_not_an_orphan() {
        // 空的登记表 + 没失败扫描 = 明确「不归任何插件」。这正是普通 WebDAV 的处境。
        let dir = temp_dir("native");
        let file = dir.join(FILE);
        let table = load_from(&file).expect("load");
        assert!(table.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_remote_whose_config_could_not_be_read_is_refused() {
        // 配置读不出来时说不清这条远程归谁。按「没登记」处理等于拿 DAV 协议去碰
        // 插件远程的数据，所以这里必须给出错误。
        let mut unreadable = BTreeMap::new();
        unreadable.insert("pic".to_string(), "远程 pic 的配置这次读不出来".to_string());
        let err = decide(&BTreeMap::new(), &BTreeSet::new(), &unreadable, None, "pic")
            .expect_err("读不回来就得拒绝");
        assert!(err.contains("pic"), "错误里要点名是哪条远程：{err}");
        assert!(decide(
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            None,
            "pic",
        )
        .expect("谁都读得回来时就是普通远程")
        .is_none());
    }

    #[test]
    fn one_unreadable_remote_does_not_drag_down_the_others() {
        let mut claims = BTreeMap::new();
        claims.insert(
            "相册".to_string(),
            Claim {
                plugin: "kvault".to_string(),
                marker: "kvault".to_string(),
                label: "K-Vault".to_string(),
                source: "declared".to_string(),
            },
        );
        let mut unreadable = BTreeMap::new();
        unreadable.insert("pic".to_string(), "读不出来".to_string());
        assert!(decide(&claims, &BTreeSet::new(), &unreadable, None, "相册")
            .expect("登记过的照常返回")
            .is_some());
        assert!(decide(&claims, &BTreeSet::new(), &unreadable, None, "dav")
            .expect("读得回来的普通远程不该受影响")
            .is_none());
        assert!(decide(&claims, &BTreeSet::new(), &unreadable, None, "pic").is_err());
    }

    #[test]
    fn a_failed_scan_means_no_remote_can_be_called_native() {
        // 扫描没跑成时，「表里没有」什么都不能证明。
        let err = decide(
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            Some("没能读出远程列表"),
            "dav",
        )
        .expect_err("扫描失败时必须拒绝");
        assert!(err.contains("远程列表"), "{err}");
    }

    #[test]
    fn an_examined_remote_is_native_even_though_the_scan_failed() {
        // 启动扫描失败后，用户真要打开一条远程时宿主会当场再读一次配置。
        // 读成功的那条已经有答案了，不该被整次扫描的失败连累——否则一条普通 WebDAV
        // 会因为一次超时被锁到下次启动，而「普通远程不受影响」是明确要求。
        let mut examined = BTreeSet::new();
        examined.insert("dav".to_string());
        let scan = Some("这次启动没能读出远程列表");
        assert!(
            decide(&BTreeMap::new(), &examined, &BTreeMap::new(), scan, "dav")
                .expect("当场读过、读成功的远程可以下结论")
                .is_none()
        );
        assert!(decide(&BTreeMap::new(), &examined, &BTreeMap::new(), scan, "pic").is_err());
    }

    fn claim(plugin: &str, marker: &str, label: &str, source: &str) -> Claim {
        Claim {
            plugin: plugin.to_string(),
            marker: marker.to_string(),
            label: label.to_string(),
            source: source.to_string(),
        }
    }

    fn rec(remote: &str, plugin: &str, marker: &str) -> RemoteOwnerRef {
        RemoteOwnerRef {
            remote: remote.to_string(),
            plugin: plugin.to_string(),
            marker: marker.to_string(),
            label: plugin.to_string(),
            source: "declared".to_string(),
        }
    }

    fn table_of(items: &[(&str, Claim)]) -> BTreeMap<String, Claim> {
        items
            .iter()
            .map(|(name, c)| (name.to_string(), c.clone()))
            .collect()
    }

    #[test]
    fn an_exported_record_carries_only_the_ownership() {
        // 导出的是「谁归谁」，不带远端内容也不带凭据：包可能被随便转存。
        let table = table_of(&[
            ("相册", claim("kvault", "kvault", "K-Vault", "legacy")),
            ("网盘", claim("other", "other_flag", "别的插件", "scan")),
        ]);
        let refs = export_refs(&table);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].remote, "相册");
        assert_eq!(refs[0].marker, "kvault");
        assert_eq!(refs[1].plugin, "other");
        assert_eq!(refs[1].source, "scan");
    }

    #[test]
    fn import_claims_a_remote_only_when_its_own_params_carry_the_marker() {
        let declared = vec![derived("kvault", "kvault")];
        // 本机参数里有标记键，插件也在：按插件现在声明的名字登记。
        let live = params(json!({"type": "webdav", "url": "https://x/dav", "kvault": "1"}));
        match decide_import(&rec("相册", "kvault", "kvault"), Some(&live), &declared) {
            ImportDecision::Claim(c) => {
                assert_eq!(c.plugin, "kvault");
                assert_eq!(c.marker, "kvault");
                assert_eq!(c.source, "import");
            }
            ImportDecision::Skip(why) => panic!("本机参数作证，应当登记：{why}"),
        }
        // 插件不在（卸载后还没装回来）：文件里的记录是唯一线索，照样登记，
        // 这样这台远程会在插件回来之前被拒绝，而不是退回原生协议。
        match decide_import(&rec("相册", "kvault", "kvault"), Some(&live), &[]) {
            ImportDecision::Claim(c) => {
                assert_eq!(c.plugin, "kvault");
                assert_eq!(c.label, "kvault");
            }
            ImportDecision::Skip(why) => panic!("插件不在时仍要按文件登记：{why}"),
        }
        // 本机这条远程的参数里没有标记键：它是普通远程，文件说什么都不能锁住它。
        let plain = params(json!({"type": "s3", "provider": "Other", "bucket": "b"}));
        match decide_import(&rec("backup", "kvault", "kvault"), Some(&plain), &declared) {
            ImportDecision::Skip(why) => assert!(why.contains("普通远程"), "{why}"),
            ImportDecision::Claim(_) => panic!("普通远程不能被文件里的记录锁住"),
        }
        // 参数读不出来：不登记，也不假装它是普通远程。
        match decide_import(&rec("相册", "kvault", "kvault"), None, &declared) {
            ImportDecision::Skip(why) => assert!(why.contains("没有远程"), "{why}"),
            ImportDecision::Claim(_) => panic!("读不到参数时不能登记"),
        }
        // 记录本身不完整。
        let broken = RemoteOwnerRef {
            remote: "相册".to_string(),
            plugin: String::new(),
            marker: "kvault".to_string(),
            label: String::new(),
            source: String::new(),
        };
        assert!(matches!(
            decide_import(&broken, Some(&live), &declared),
            ImportDecision::Skip(_)
        ));
    }

    #[test]
    fn two_records_for_one_remote_end_up_with_one_claim() {
        // 文件里同一条远程被写了两遍（手改、或两个旧包合并）：表里只能有一条。
        let declared = vec![derived("kvault", "kvault")];
        let mut live: BTreeMap<String, Option<Map<String, Value>>> = BTreeMap::new();
        live.insert(
            "相册".to_string(),
            Some(params(json!({"kvault": "1", "url": "https://x/dav"}))),
        );
        let mut second = rec("相册", "kvault", "kvault");
        second.plugin = "kvault".to_string();
        let merged = merge_imported(
            &[rec("相册", "kvault", "kvault"), second],
            &["相册".to_string()].into_iter().collect(),
            &live,
            &declared,
            &BTreeMap::new(),
        );
        assert_eq!(merged.table.len(), 1);
        assert_eq!(merged.claimed, 1, "重复记录只算一次登记");
        assert_eq!(merged.table["相册"].plugin, "kvault");
    }

    #[test]
    fn importing_does_not_touch_claims_for_other_remotes() {
        // 只增不删：包里没提的远程，本机登记原样留着（比如另一个插件、另一个账户）。
        let declared = vec![derived("kvault", "kvault")];
        let current = table_of(&[(
            "别的相册",
            claim("other", "other_flag", "别的插件", "declared"),
        )]);
        let mut live: BTreeMap<String, Option<Map<String, Value>>> = BTreeMap::new();
        live.insert("相册".to_string(), Some(params(json!({"kvault": "1"}))));
        let merged = merge_imported(
            &[rec("相册", "kvault", "kvault")],
            &["相册".to_string()].into_iter().collect(),
            &live,
            &declared,
            &current,
        );
        assert_eq!(merged.claimed, 1);
        assert_eq!(
            merged.table.get("别的相册"),
            current.get("别的相册"),
            "包外远程的登记不能被动"
        );
    }

    #[test]
    fn a_package_that_lists_a_plain_remote_drops_the_stale_claim() {
        // 文件里这台是普通远程、本机参数里也没有标记键：说明用户已经把它改成普通
        // 远程了。旧登记留着会让它一直打不开。
        let declared: Vec<Derived> = Vec::new();
        let current = table_of(&[("dav", claim("kvault", "kvault", "K-Vault", "legacy"))]);
        let mut live: BTreeMap<String, Option<Map<String, Value>>> = BTreeMap::new();
        live.insert(
            "dav".to_string(),
            Some(params(json!({"type": "webdav", "url": "https://x/dav"}))),
        );
        let merged = merge_imported(
            &[],
            &["dav".to_string()].into_iter().collect(),
            &live,
            &declared,
            &current,
        );
        assert!(merged.table.is_empty(), "旧登记应当被撤掉");
        assert_eq!(merged.dropped, 1);
        assert!(
            merged.notes.iter().any(|n| n.contains("原来的登记撤掉了")),
            "{:?}",
            merged.notes
        );
    }

    #[test]
    fn an_unreadable_remote_keeps_its_claim_and_says_so() {
        // 参数没读到，就什么都不能改：宁可不放开，也不要有一次「悄悄退回原生」。
        let declared: Vec<Derived> = Vec::new();
        let current = table_of(&[("相册", claim("kvault", "kvault", "K-Vault", "declared"))]);
        let mut live: BTreeMap<String, Option<Map<String, Value>>> = BTreeMap::new();
        live.insert("相册".to_string(), None);
        let merged = merge_imported(
            &[],
            &["相册".to_string()].into_iter().collect(),
            &live,
            &declared,
            &current,
        );
        assert_eq!(
            merged.table.get("相册"),
            current.get("相册"),
            "读不到参数时登记表一个字节都不能改"
        );
        assert_eq!(
            merged.dropped, 0,
            "读不到参数不等于「用户把它改成普通远程」，一条都不能撤"
        );
        // 话要说全：点名哪台远程、说清它还归谁、以及这次没动它。不逐字比对整句——
        // 那句话是给人看的，改词不该让行为测试红；但少说其中任何一条都要红。
        assert_eq!(
            merged.notes.len(),
            1,
            "这台远程只该有一条说明：{:?}",
            merged.notes
        );
        let note = &merged.notes[0];
        assert!(note.contains("相册"), "要点名是哪台远程：{note}");
        assert!(note.contains("K-Vault"), "要说清它还归谁管：{note}");
        assert!(note.contains("没有动"), "要说清这次没改它：{note}");
    }

    #[test]
    fn a_changed_plugin_is_reported_when_the_ownership_moves() {
        // 同一条远程换了归属：要说出原来是谁，否则用户不知道发生了什么。
        let declared = vec![derived("kvault", "kvault")];
        let current = table_of(&[("相册", claim("old", "kvault", "旧插件", "declared"))]);
        let mut live: BTreeMap<String, Option<Map<String, Value>>> = BTreeMap::new();
        live.insert("相册".to_string(), Some(params(json!({"kvault": "1"}))));
        let merged = merge_imported(
            &[rec("相册", "kvault", "kvault")],
            &["相册".to_string()].into_iter().collect(),
            &live,
            &declared,
            &current,
        );
        assert_eq!(merged.table["相册"].plugin, "kvault");
        assert!(
            merged
                .notes
                .iter()
                .any(|n| n.contains("旧插件") && n.contains("kvault")),
            "{:?}",
            merged.notes
        );
    }

    #[test]
    fn the_table_is_backed_up_before_it_is_rewritten() {
        // 覆盖之前先留一份，而且坏掉的那份也照原样留着——用户要用它手工修复。
        let dir = temp_dir("backup");
        let file = dir.join(FILE);
        let broken = b"{ not json at all";
        std::fs::write(&file, broken).expect("write");
        let table = table_of(&[("相册", claim("kvault", "kvault", "K-Vault", "declared"))]);
        let bak = save_with_backup(&file, &table)
            .expect("写回成功")
            .expect("原文件存在时应当有备份");
        assert_eq!(std::fs::read(&bak).expect("读备份"), broken);
        let back = load_from(&file).expect("新表可读");
        assert_eq!(back.get("相册"), table.get("相册"));
        // 文件本来不存在时没有备份可做。
        let fresh = dir.join("fresh.json");
        assert!(save_with_backup(&fresh, &table).expect("写新表").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
