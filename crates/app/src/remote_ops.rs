//! 远程操作路由：一条远程归谁管，归插件的就交给插件。
//!
//! 主程序不知道任何具体插件的业务，也不从参数里猜「这是不是插件远程」。
//! 归属来自一张登记表（`crate::remote_owners`），只有三处会写它：用户保存一条
//! 插件类型的远程、整体导入备份、升级后第一次启动扫一遍旧配置。
//!
//! 于是路由只有两步：
//!
//! 1. 查登记表：这条远程归哪个插件，或者明确不属于任何插件；
//! 2. 归插件的，按插件清单里声明过的操作名把请求转过去。登记了但认领者不在
//!    （插件停用、卸载、插件目录被删），**明确拒绝**。
//!
//! 第 2 条是这个模块存在的主要理由。插件远程不是「普通 `base_type` 加几个参数」：
//! 它可能指向另一个协议的接口、要带特殊的认证头、目录结构也不是文件系统那样。
//! 这时候退回 `base_type` 的原生访问，等于拿一个语义不同的协议去读写用户的数据：
//! 写进去的东西对方不认，读出来的东西是错的，而且用户看不到任何异常。
//!
//! 反过来，**没登记**的远程完全不受影响：它们走不到这里，仍然按 `base_type` 的
//! 原生方式访问，插件装没装都一样。参数里有陌生键也不会让一条普通远程被当成
//! 插件远程——判断只看登记表。
//!
//! 登记表读不出来时这里返回错误，而不是「没有登记」：那时候说不清谁管这条远程，
//! 唯一安全的做法是拒绝并说明原因。

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::config;
use crate::remote_owners::Claim;
use crate::remote_types::{self, Derived};
use crate::Ctx;

/// 清单里声明能接管的操作名。和 `plugin_host::config::REMOTE_OPS` 是同一组名字，
/// 这里再写一遍是为了让宿主侧的匹配有类型，不必到处写字面量。
///
/// 这些名字会原样成为插件的 `action`，所以必须和插件面板上的动作名区分开：
/// 面板里已经有一个叫 `list` 的动作（列出站点索引给人看），宿主发 `remote.list`
/// 才不会打错门。
pub const LIST: &str = "remote.list";
pub const UPLOAD: &str = "remote.upload";
pub const DELETE: &str = "remote.delete";
pub const WRITE: &str = "remote.rclone-write";

/// 一条远程现在归谁管。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// 参数里没有任何标记键：普通远程，走内核的原生实现。
    Native,
    /// 有启用的插件认领，并且它声明了要接管的操作（可能一个都没声明）。
    Plugin {
        id: String,
        name: String,
        ops: Vec<String>,
    },
    /// 参数里带了标记，但没有任何启用的插件认领这个标记。
    ///
    /// `marker` 是命中的那个键名，只用来写提示；不拿它去猜插件是谁——
    /// 猜错会把用户引到一个不相干的插件设置页。
    Orphan { marker: String },
}

impl Owner {
    /// 这条远程是不是由插件认领的。`Orphan` 也算：它**曾经**是插件的。
    pub fn claimed(&self) -> bool {
        !matches!(self, Owner::Native)
    }

    /// 认领者是否声明了能承接 `op`。
    pub fn takes(&self, op: &str) -> bool {
        matches!(self, Owner::Plugin { ops, .. } if ops.iter().any(|o| o == op))
    }

    /// 认领插件的 id（`Native`/`Orphan` 没有）。
    pub fn plugin_id(&self) -> Option<&str> {
        match self {
            Owner::Plugin { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// 按「已启用插件的声明」判断一份参数归谁。
///
/// 纯函数。**只在登记时使用**（见 `crate::remote_owners::claim_of`）：运行期不能
/// 拿它判断归属，因为插件一停用，它的声明就不在表里了，插件远程会看起来和普通
/// 远程一模一样。
pub fn owner_in(table: &[Derived], params: &Map<String, Value>) -> Owner {
    match remote_types::owner_of_params(table, params) {
        Some(d) => Owner::Plugin {
            id: d.plugin.clone(),
            name: d.name.clone(),
            ops: d.ops.clone(),
        },
        None => Owner::Native,
    }
}

/// 一条登记记录落到当前插件表上的结果。
///
/// 插件在场且声明的还是同一个标记 ⇒ 由它接管；不在或改了标记 ⇒ `Orphan`。
/// 后一种情况绝不能退回原生访问，所以这里没有「查不到就当普通远程」的分支。
pub fn resolve_in_one(claim: &Claim, table: &[Derived]) -> Owner {
    match table
        .iter()
        .find(|d| d.plugin == claim.plugin && d.marker == claim.marker)
    {
        Some(d) => Owner::Plugin {
            id: d.plugin.clone(),
            name: d.name.clone(),
            ops: d.ops.clone(),
        },
        None => Owner::Orphan {
            marker: claim.marker.clone(),
        },
    }
}

/// 按整张登记表判断一条远程归谁。纯函数，测试直接喂它。
pub fn resolve_in(claims: &BTreeMap<String, Claim>, table: &[Derived], remote: &str) -> Owner {
    match claims.get(remote.trim()) {
        Some(c) => resolve_in_one(c, table),
        None => Owner::Native,
    }
}

/// 「这条远程的认领者不在了」的提示。不含任何具体插件的名字。
fn orphan_msg(remote: &str, marker: &str) -> String {
    rcgui_i18n::f(
        "远程 {} 的配置里带着 {} 标记，但它对应的插件现在没有启用，所以这个远程打不开。\
         请到「设置 → 插件」里把那个插件启用；远程配置和远端的数据都没有动过。",
        &[&remote, &marker],
    )
}

/// 认领者声明了类型、但没声明要接管这个操作。
fn not_taken_msg(remote: &str, op: &str, name: &str) -> String {
    rcgui_i18n::f(
        "远程 {} 由插件「{}」提供，但那个插件没有声明要接管「{}」，主程序不会替它猜怎么做。",
        &[&remote, &name, &op],
    )
}

/// 同 [`orphan_msg`]，给「先判归属、再自己决定怎么说」的调用方用。
///
/// 写入口（上传、删除、新建目录）在分派前就拿到归属了；它们要把提示拼进自己的
/// 上下文（比如删三个文件时只说一次），所以需要一个能取到这句话的公开入口。
pub fn orphan_message(remote: &str, marker: &str) -> String {
    orphan_msg(remote, marker)
}

/// 判断这条远程归谁管：只查登记表，不读内核里的参数。
///
/// 登记表读不出来时返回 `Err`，调用方必须拒绝操作并说明原因——那种时候说不出
/// 这条远程是谁的，按原生方式访问等于拿错误的协议去读写用户的数据。
pub async fn owner_of(ctx: &Arc<Ctx>, remote: &str) -> Result<Owner, String> {
    let name = remote.trim();
    let table = remote_types::collect(ctx);
    let claim = match crate::remote_owners::resolve(name) {
        Ok(c) => c,
        Err(first) => {
            // 启动那次扫描没读到它的配置（或整次没跑成）：现在当场就这一条再读一次。
            // 读得回来就能按实际配置判断，用不着把一条普通远程锁到下次启动；
            // 读不回来才把「说不清它归谁」告诉用户。
            match crate::remote_owners::note_remote(ctx, name).await {
                Ok(()) => crate::remote_owners::resolve(name).map_err(|e| e.to_string())?,
                Err(_) => return Err(first),
            }
        }
    };
    Ok(match claim {
        Some(c) => resolve_in_one(&c, &table),
        None => Owner::Native,
    })
}

/// 拦截原因：这条远程由插件提供、而插件现在不在（或登记表读不出来），返回一句人话。
///
/// 返回 `None` 表示可以继续——普通远程，或者认领的插件在工作。
pub async fn blocked(ctx: &Arc<Ctx>, remote: &str) -> Option<String> {
    match owner_of(ctx, remote).await {
        Ok(Owner::Orphan { marker }) => Some(orphan_msg(remote, &marker)),
        Ok(_) => None,
        Err(e) => Some(e),
    }
}

/// 一次算好一份「远程名 → 拦截原因」，给批量场景用（启动恢复、定时核对）。
///
/// 只判 `cfg` 里真的出现过的远程：没被任何挂载/服务引用的远程不该出现在提示里。
/// 登记表读不出来时，`cfg` 里每个远程都拿到同一条原因——批量路径（挂载、服务、
/// 同步）不能因为「查不到登记」就按原生方式继续跑。
pub async fn blocked_map(ctx: &Arc<Ctx>, cfg: &config::AppConfig) -> HashMap<String, String> {
    let mut names: Vec<String> = Vec::new();
    for m in &cfg.auto_mounts {
        names.push(m.remote.clone());
    }
    for s in &cfg.auto_serves {
        names.push(s.remote.clone());
    }
    names.retain(|n| !n.trim().is_empty());
    names.sort();
    names.dedup();

    let table = remote_types::collect(ctx);
    let claims = match crate::remote_owners::load() {
        Ok(c) => c,
        Err(e) => return names.into_iter().map(|n| (n, e.clone())).collect(),
    };
    let mut out = HashMap::new();
    for name in names {
        if claims.contains_key(&name) {
            if let Owner::Orphan { marker } = resolve_in(&claims, &table, &name) {
                // 先把要说的话拼好，再把名字交进 map：`insert` 会拿走 `name`，
                // 写成 `insert(name, orphan_msg(&name, …))` 就是先移动后借用。
                let why = orphan_msg(&name, &marker);
                out.insert(name, why);
            }
            continue;
        }
        // 没登记的名字还要问一次 `resolve`：本次启动的历史扫描失败时，
        // 「没登记」不等于「不归插件」，这种情况必须拦住。
        if let Err(e) = crate::remote_owners::resolve(&name) {
            out.insert(name, e);
        }
    }
    out
}

/// 写类操作（改名、移动、复制、建目录）在任何插件管的远程上都做不了。
///
/// 插件声明了 `rclone-write` 也一样：这些动作要由插件给出「怎么表达一次改名」，
/// 而站点式的接口根本没有这个概念。与其让主程序编一个，不如直接说不行。
pub async fn write_blocked(ctx: &Arc<Ctx>, remote: &str) -> Option<String> {
    match owner_of(ctx, remote).await {
        Ok(Owner::Native) => None,
        Ok(Owner::Orphan { marker }) => Some(orphan_msg(remote, &marker)),
        Ok(Owner::Plugin { name, .. }) => Some(rcgui_i18n::f(
            "远程 {} 由插件「{}」提供，它不支持改名、移动、新建目录：这类改动要到对应的\
             网页端或客户端里做。",
            &[&remote, &name],
        )),
        Err(e) => Some(e),
    }
}

/// 把 op 转给认领的插件。`extra` 会并进插件的 `params`。
async fn forward(
    ctx: &Arc<Ctx>,
    owner: &Owner,
    remote: &str,
    op: &str,
    extra: Value,
) -> Result<Value, String> {
    let id = match owner {
        Owner::Plugin { id, ops, name } => {
            if !ops.iter().any(|o| o == op) {
                return Err(not_taken_msg(remote, op, name));
            }
            id.clone()
        }
        Owner::Orphan { marker } => return Err(orphan_msg(remote, marker)),
        Owner::Native => return Err(not_taken_msg(remote, op, "（原生）")),
    };
    crate::plugins::call_plugin(ctx, &id, op, extra)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// 列一层目录。
///
/// `Ok(None)` 表示不归插件管，调用方接着走原生；`Err` 表示归属判断不出来
/// （登记表坏了、启动扫描失败），调用方必须把这个原因显示给用户而不是继续。
pub async fn list(
    ctx: &Arc<Ctx>,
    remote: &str,
    path: &str,
    sort_key: &str,
    desc: bool,
) -> Result<Option<Vec<FileEntry>>, String> {
    let owner = owner_of(ctx, remote).await?;
    if !owner.claimed() {
        return Ok(None);
    }
    let extra = json!({ "remote": remote, "path": path, "sort": sort_key, "desc": desc });
    let v = forward(ctx, &owner, remote, LIST, extra).await?;
    parse_entries(&v).map(Some)
}

/// 插件回给宿主的目录条目。字段名与 `remote_types` 无关，是 `remote.list` 的约定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// RFC3339；空串表示插件不知道。
    pub uploaded: String,
}

fn parse_entries(v: &Value) -> Result<Vec<FileEntry>, String> {
    let arr = match v {
        Value::Array(a) => a,
        Value::Object(m) => match m.get("items") {
            Some(Value::Array(a)) => a,
            _ => return Err("插件返回的目录内容不是列表".to_string()),
        },
        Value::Null => return Ok(Vec::new()),
        _ => return Err("插件返回的目录内容不是列表".to_string()),
    };
    let mut out = Vec::with_capacity(arr.len());
    for it in arr {
        let name = it.get("name").and_then(Value::as_str).unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        out.push(FileEntry {
            name: name.to_string(),
            is_dir: it.get("is_dir").and_then(Value::as_bool).unwrap_or(false),
            size: it.get("size").and_then(Value::as_u64).unwrap_or(0),
            uploaded: it
                .get("uploaded")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        });
    }
    Ok(out)
}

/// 一次上传的汇总。目录展开、文案都在宿主（那是本地文件系统的事），
/// 只有「这一个文件怎么发出去」归插件。
#[derive(Default, Debug)]
pub struct Uploaded {
    pub total: usize,
    pub ok: usize,
    pub failed: Vec<String>,
    pub link: String,
    pub name: String,
    pub single: bool,
    pub dedup: usize,
}

impl Uploaded {
    /// 全成功时 `(1, 人话)`，有失败时 `(2, 人话)`。和 `toast` 的 level 对齐。
    pub fn report(&self, where_: &str) -> (i32, String) {
        if self.failed.is_empty() {
            let mut s = rcgui_i18n::f("已上传 {} 个文件到 {}", &[&self.ok.to_string(), &where_]);
            if self.dedup > 0 {
                s.push_str(&rcgui_i18n::f(
                    "；其中 {} 个远端已有同一份内容，直接复用了直链",
                    &[&self.dedup.to_string()],
                ));
            }
            (1, s)
        } else {
            let head = rcgui_i18n::f(
                "上传完成 {}/{}，失败 {}：{}",
                &[
                    &self.ok.to_string(),
                    &self.total.to_string(),
                    &self.failed.len().to_string(),
                    &self
                        .failed
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("；"),
                ],
            );
            let more = if self.failed.len() > 3 {
                rcgui_i18n::f(
                    "（另有 {} 条未列出）",
                    &[&(self.failed.len() - 3).to_string()],
                )
            } else {
                String::new()
            };
            (2, format!("{head}{more}"))
        }
    }
}

/// 把一批本地路径发到插件管的远程。
///
/// 每个文件单独授权给认领的插件（`http.upload` 的授权表按 `(插件, 栏位)`
/// 记，一次挑选只放行一次发送），然后由插件自己拼 URL 与表单字段。
/// 宿主只递过去「哪个文件、发到哪个目录、叫什么名字」。
pub async fn upload_paths(
    ctx: &Arc<Ctx>,
    remote: &str,
    dir: &str,
    paths: &[std::path::PathBuf],
) -> Result<Uploaded, String> {
    let owner = owner_of(ctx, remote).await?;
    let plugin = match owner.plugin_id() {
        Some(id) if owner.takes(UPLOAD) => id.to_string(),
        Some(_) => return Err(not_taken_msg(remote, UPLOAD, "该插件")),
        None => {
            return Err(match &owner {
                Owner::Orphan { marker } => orphan_msg(remote, marker),
                _ => not_taken_msg(remote, UPLOAD, "（原生）"),
            })
        }
    };

    // 目录展开：本地文件系统的事，留在宿主。第三个字段是「相对这次上传根目录的
    // 子目录」，拖进来一个文件夹时，里面的层级要照原样落到远端，不能摊平成一堆同名文件。
    let jobs = expand(paths);
    let mut up = Uploaded {
        total: jobs.len(),
        single: jobs.len() == 1,
        ..Default::default()
    };
    if up.total == 0 {
        return Ok(up);
    }

    for (i, (abs, fname, rel)) in jobs.into_iter().enumerate() {
        // 授权只对这一栏、这一次有效。授权失败（文件刚被删、不是普通文件）
        // 就当这一个失败，不动别的。栏位名带上序号：同一个文件夹里两层的同名文件
        // 否则会共用一格授权。
        let key = format!("remote-{i}-{fname}");
        if let Err(e) = plugin_host::upload::grant(&plugin, &key, &abs) {
            up.failed.push(format!("{}: {e}", fname));
            continue;
        }
        let extra = json!({
            "remote": remote,
            "path": join_dir(dir, &rel),
            "file_name": fname,
            "file_key": key,
        });
        match forward(ctx, &owner, remote, UPLOAD, extra).await {
            Ok(v) => {
                let name = v
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&fname)
                    .to_string();
                if v.get("deduped").and_then(Value::as_bool).unwrap_or(false) {
                    up.dedup += 1;
                }
                // 名字记下来是为了「直链已复制」那句提示能说出是哪个文件；
                // 直链只有站点给了才有，去重命中时通常没有。
                up.name = name;
                if let Some(link) = v.get("link").and_then(Value::as_str) {
                    if !link.is_empty() {
                        up.link = link.to_string();
                    }
                }
                up.ok += 1;
            }
            Err(e) => {
                // 授权在发送前就被取走了，失败后必须重新挑，插件不会自己重试。
                plugin_host::upload::revoke(&plugin, &key);
                up.failed.push(format!("{fname}: {e}"));
            }
        }
    }
    Ok(up)
}

/// 从 rclone 的 `远程:路径` 里取出远程名。
///
/// 只有第一段冒号前面才算远程名，而且得像个名字。返回 `None` 表示这一定是本机路径，
/// 不用问归属。
///
/// 判「一定是本机路径」的标准是盘符后面紧跟分隔符（`C:\img\a.png`、`C:/img/a.png`）：
/// 那是 Windows 绝对路径，rclone 的 `fs` 不会这么写。单个字母后面不跟分隔符的
/// （`x:`、`x:album`）两头都可能——既是盘符相对路径的样子，也可能真是一条叫 `x` 的
/// 远程。这里不替用户拍板，交回 `Some` 让归属去问：没人认领才按原生走，
/// 有插件认领就必须走它，不能因为「看着像盘符」把一条插件远程当成原生写入。
fn remote_name_of(fs: &str) -> Option<&str> {
    let (name, rest) = fs.split_once(':')?;
    let name = name.trim();
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return None;
    }
    let drive_like = name.len() == 1 && name.chars().all(|c| c.is_ascii_alphabetic());
    if drive_like && (rest.starts_with('\\') || rest.starts_with('/')) {
        return None;
    }
    Some(name)
}

/// 一次上传专属的授权键。
///
/// 授权表按 `(插件, 键)` 记账，`grant` 是覆盖式写入：两条并发上传用同一个键，后一条会
/// 顶掉前一条，插件回来取字节时拿到的是别人的文件，撤销时也会误撤别人那条。所以键里
/// 必须带一个进程内唯一的流水号。
///
/// 只能用 ASCII 字母数字和 `.`/`_`/`-`（`plugin_host::upload::is_key` 的约束），远端
/// 文件名因此不能直接当键：中文名会让这条授权根本建不起来。
static GRANT_SEQ: AtomicU64 = AtomicU64::new(0);

fn grant_key(seq: u64) -> String {
    format!("cap-{}-{}", std::process::id(), seq)
}

fn next_grant_key() -> String {
    grant_key(GRANT_SEQ.fetch_add(1, Ordering::SeqCst))
}

/// 一条授权从建立到撤销的全权负责人。
///
/// 插件真正发送时会自己把授权取走（`claim`），那时撤销是空操作；留着它是为了
/// 「发起方没走到发送那一步」的路径——插件缺席、协议报错、或者调用方中途把 future
/// 丢掉（取消）。这三种情况下授权都不该在表里留到过期。
struct GrantGuard {
    plugin: String,
    key: String,
}

impl Drop for GrantGuard {
    fn drop(&mut self) {
        plugin_host::upload::revoke(&self.plugin, &self.key);
    }
}

/// 这次上传该由谁发字节。
///
/// `Ok(None)` 是普通远程，调用方照旧走 rclone；`Ok(Some(id))` 是托管这条远程的插件；
/// `Err` 是必须原样告诉用户的原因——认领者缺席时绝不能退回原生访问，那等于拿错协议
/// 去写用户的站点。
fn pick_uploader(owner: &Owner, remote: &str) -> Result<Option<String>, String> {
    match owner {
        Owner::Native => Ok(None),
        Owner::Orphan { marker } => Err(orphan_msg(remote, marker)),
        Owner::Plugin { id, .. } if owner.takes(UPLOAD) => Ok(Some(id.clone())),
        Owner::Plugin { name, .. } => Err(not_taken_msg(remote, UPLOAD, name)),
    }
}

/// 目标位置拆成「目录 + 文件名」。
///
/// 拼法跟 rclone 的 `copyfile` 一致：`remote_path` 相对 `fs` 的路径部分。换条路走插件
/// 时口径不能变，否则同一个请求在两种远程上会落到不同位置。
fn target_of(fs: &str, remote_path: &str, file_name: &str) -> (String, String) {
    let base = fs.split_once(':').map(|(_, p)| p).unwrap_or("");
    let rel = if remote_path.trim().is_empty() {
        file_name
    } else {
        remote_path
    };
    let full = join_dir(base, rel);
    if full.is_empty() {
        return (String::new(), file_name.to_string());
    }
    match full.rfind('/') {
        Some(i) => (full[..i].to_string(), full[i + 1..].to_string()),
        None => (String::new(), full),
    }
}

/// 把已经落在临时文件里的内容交给托管这条远程的插件发送。
///
/// `Ok(None)` 表示这条远程不归插件管，调用方该走原生 `copyfile`；`Ok(Some(v))` 是插件
/// 的回复，里面带着它自己算出的 `name`/`link`/`deduped`（K-Vault 走站点接口，只有它
/// 知道直链长什么样）。`Err` 会在用户面前原样出现：认领者没启用、或者声明不接上传，
/// 都在这里被挡住，不会退回到「用错协议写一遍」。
///
/// 字节只走一次：`src` 按 [`plugin_host::upload::grant`] 的规矩授权给插件，插件发完
/// 就作废；没走到发送的路径由 [`GrantGuard`] 收尾，不会留到过期。
pub async fn upload_file_owned(
    ctx: &Arc<Ctx>,
    fs: &str,
    remote_path: &str,
    file_name: &str,
    src: &std::path::Path,
) -> Result<Option<Value>, String> {
    let Some(remote) = remote_name_of(fs) else {
        return Ok(None);
    };
    let owner = owner_of(ctx, remote).await?;
    let Some(plugin) = pick_uploader(&owner, remote)? else {
        return Ok(None);
    };
    let (dir, name) = target_of(fs, remote_path, file_name);
    let key = next_grant_key();
    plugin_host::upload::grant(&plugin, &key, src).map_err(|e| format!("{e}"))?;
    // 从这里开始，这条授权由 guard 负责撤销：成功时插件已经取走（撤销是空操作），
    // 失败和取消都由 Drop 兜住。
    let _guard = GrantGuard {
        plugin: plugin.clone(),
        key: key.clone(),
    };
    let extra = json!({
        "remote": remote,
        "path": dir,
        "file_name": name,
        "file_key": key,
    });
    forward(ctx, &owner, remote, UPLOAD, extra).await.map(Some)
}

/// 把路径拆成「要发的文件」。目录只展一层还是递归，是宿主的选择：
/// 用户拖进来的文件夹照惯例是整个传上去，所以递归。
///
/// 第三个字段是「相对这次上传根目录的子目录」：拖进来一个文件夹时，里面的层级
/// 要照原样落到远端，否则三层目录的文件全被摊进同一层，重名的还会互相顶掉。
fn expand(paths: &[std::path::PathBuf]) -> Vec<(std::path::PathBuf, String, String)> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            walk(p, "", 0, &mut out);
        } else if p.is_file() {
            if let Some(n) = p.file_name().and_then(|s| s.to_str()) {
                out.push((p.clone(), n.to_string(), String::new()));
            }
        }
    }
    out
}

fn walk(
    dir: &std::path::Path,
    rel: &str,
    depth: usize,
    out: &mut Vec<(std::path::PathBuf, String, String)>,
) {
    // 深度与条数都封顶：符号链接成环时这里会一直转下去，而用户等的是「传完了没」。
    if depth > 32 || out.len() > 20_000 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let name = match p.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if meta.is_dir() {
            walk(&p, &join_dir(rel, &name), depth + 1, out);
        } else if meta.is_file() {
            out.push((p, name, rel.to_string()));
        }
    }
}

/// 拼远端目录。空串表示「就在根目录下」，不要拼出一个多余的 `/`。
fn join_dir(dir: &str, rel: &str) -> String {
    let dir = dir.trim().trim_matches('/');
    let rel = rel.trim().trim_matches('/');
    match (dir.is_empty(), rel.is_empty()) {
        (_, true) => dir.to_string(),
        (true, false) => rel.to_string(),
        (false, false) => format!("{dir}/{rel}"),
    }
}

/// 一次删除的汇总。
///
/// 三份都是**名字**而不是计数：删完要把还留着的那几行从列表里摘掉，只凭计数
/// 没法知道摘谁。
#[derive(Default, Debug)]
pub struct Deleted {
    pub gone: Vec<String>,
    pub missing: Vec<String>,
    pub failed: Vec<String>,
}

/// 删掉当前目录里的若干项。`items` 是 `(名字, 是不是目录)`。
pub async fn delete(
    ctx: &Arc<Ctx>,
    remote: &str,
    dir: &str,
    items: &[(String, bool)],
) -> Result<Deleted, String> {
    let owner = owner_of(ctx, remote).await?;
    if !owner.takes(DELETE) {
        return Err(match &owner {
            Owner::Orphan { marker } => orphan_msg(remote, marker),
            Owner::Plugin { name, .. } => not_taken_msg(remote, DELETE, name),
            Owner::Native => not_taken_msg(remote, DELETE, "（原生）"),
        });
    }
    let extra = json!({
        "remote": remote,
        "path": dir,
        "items": items.iter().map(|(n, d)| json!({"name": n, "is_dir": d})).collect::<Vec<_>>(),
    });
    let v = forward(ctx, &owner, remote, DELETE, extra).await?;

    let mut out = Deleted::default();
    for (field, slot) in [
        ("deleted", &mut out.gone),
        ("missing", &mut out.missing),
        ("failed", &mut out.failed),
    ] {
        if let Some(a) = v.get(field).and_then(Value::as_array) {
            slot.extend(a.iter().filter_map(|x| x.as_str().map(str::to_string)));
        }
    }
    Ok(out)
}

/// 旧数据移交：把早期宿主代管的设置交给插件。
///
/// 只在**启用的**插件声明了 `adopt_legacy` 时做，命名空间名字全来自清单；
/// 宿主只读不删：插件写坏了还能重来，用户的旧配置也不会因为一次失败就消失。
/// 插件拿不到别的插件的命名空间（`kv_get_all` 只认自己的 id），所以这一手
/// 只能由宿主发起。
pub async fn adopt_legacy(ctx: &Arc<Ctx>) -> Vec<String> {
    let table = remote_types::collect(ctx);
    let mut said = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for d in &table {
        for ns in &d.legacy {
            if seen.contains(ns) {
                continue;
            }
            seen.push(ns.clone());
            // 已经移交过的记在插件自己的状态里（键名固定，插件不需要知道）。
            let key = "__legacy_adopted";
            let state: Value = ctx
                .store
                .get_all(&d.plugin)
                .get(key)
                .cloned()
                .unwrap_or(Value::Null);
            let adopted: Vec<String> = state
                .get(ns.as_str())
                .and_then(Value::as_str)
                .map(|s| s.split(',').map(str::to_string).collect())
                .unwrap_or_default();
            if adopted.iter().any(|a| a == "1") {
                continue;
            }
            let data: Value = ctx.store.get_all(ns);
            if data.as_object().map(|m| m.is_empty()).unwrap_or(true) {
                continue;
            }
            let extra = json!({ "legacy": data, "namespace": ns });
            match crate::plugins::call_plugin(ctx, &d.plugin, "adopt_legacy", extra).await {
                // 只有确认插件真的把东西写进它自己的库，才记账。
                //
                // 插件那一轮拿不到写后状态（`kv.set` 在它回完 result 之后才执行），
                // 所以「存了几条」这件事得由宿主自己核对：插件报出它这次写了哪些键，
                // 宿主去插件的命名空间里逐个看还在不在。核不上就不记账——记账等于
                // 判旧数据死刑，下次启动也不会再试。宿主读的这份 `data` 一直留在原处。
                Ok(v) => {
                    let took = v.get("taken").and_then(Value::as_u64).unwrap_or_default();
                    let wrote: Vec<String> = v
                        .get("stored_keys")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str())
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    let after = ctx.store.get_all(&d.plugin);
                    let landed = wrote
                        .iter()
                        .filter(|k| !matches!(after.get(k.as_str()), None | Some(Value::Null)))
                        .count();
                    if landed < wrote.len() {
                        said.push(rcgui_i18n::f(
                            "插件「{}」说存下了 {} 里的 {} 项，但只在它自己的库里找到 {} 项，这次不记账，下次启动会再试",
                            &[&d.name, ns, &wrote.len().to_string(), &landed.to_string()],
                        ));
                        continue;
                    }
                    let mut now = state.clone();
                    if !now.is_object() {
                        now = json!({});
                    }
                    if let Value::Object(m) = &mut now {
                        m.insert(ns.clone(), Value::String("1".into()));
                    }
                    let _ = ctx.store.set(&d.plugin, key, now);
                    said.push(if took > 0 {
                        rcgui_i18n::f("已把 {} 的历史设置交给插件「{}」", &[ns, &d.name])
                    } else {
                        rcgui_i18n::f(
                            "插件「{}」确认 {} 里的设置它都有了，不再重复交接",
                            &[ns, &d.name],
                        )
                    });
                }
                Err(e) => said.push(rcgui_i18n::f(
                    "把 {} 的历史设置交给插件「{}」时失败：{:#}",
                    &[ns, &d.name, &e.to_string()],
                )),
            }
        }
    }
    said
}

/// 一条远程被删掉之后，把插件当年替它存的历史设置一起清掉。
///
/// 命名空间名单同样来自插件清单（`adopt_legacy` 声明的是「这些命名空间里的键就是
/// 远程名」），宿主不认识任何一个具体名字，也就不必跟着插件的实现改。
/// 插件此刻不在也照清：那些键只以远程名为键，远程没了就是悬空数据。
pub fn forget_legacy(ctx: &Arc<Ctx>, remote: &str) {
    for d in remote_types::collect(ctx) {
        for ns in &d.legacy {
            let _ = ctx.store.delete(ns, remote);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn table() -> Vec<Derived> {
        vec![
            Derived {
                name: "插件甲".into(),
                id: "alpha".into(),
                base_type: "webdav".into(),
                defaults: BTreeMap::new(),
                plugin: "alpha".into(),
                marker: "alpha".into(),
                ops: vec![LIST.into(), DELETE.into()],
                legacy: Vec::new(),
            },
            Derived {
                name: "插件乙".into(),
                id: "beta".into(),
                base_type: "sftp".into(),
                defaults: BTreeMap::new(),
                plugin: "beta".into(),
                marker: "b_flag".into(),
                ops: Vec::new(),
                legacy: Vec::new(),
            },
        ]
    }

    fn params(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    fn claim(plugin: &str, marker: &str) -> Claim {
        Claim {
            plugin: plugin.into(),
            marker: marker.into(),
            label: format!("插件{plugin}"),
            source: "declared".into(),
        }
    }

    fn registry(pairs: &[(&str, &str, &str)]) -> BTreeMap<String, Claim> {
        pairs
            .iter()
            .map(|(remote, plugin, marker)| ((*remote).to_string(), claim(plugin, marker)))
            .collect()
    }

    #[test]
    fn a_plain_remote_is_native() {
        let p =
            params(json!({"type": "webdav", "url": "https://example.org/dav", "vendor": "other"}));
        assert_eq!(owner_in(&table(), &p), Owner::Native);
        assert_eq!(resolve_in(&registry(&[]), &table(), "pic"), Owner::Native);
    }

    #[test]
    fn the_declared_marker_is_what_gets_registered() {
        // 登记时按插件声明的标记认一次，之后运行期只看登记表。
        let p = params(json!({"type": "webdav", "alpha": "1"}));
        match owner_in(&table(), &p) {
            Owner::Plugin { id, name, ops } => {
                assert_eq!(id, "alpha");
                assert_eq!(name, "插件甲");
                assert_eq!(ops, vec![LIST.to_string(), DELETE.to_string()]);
            }
            other => panic!("应当认领：{other:?}"),
        }
    }

    #[test]
    fn a_marker_key_is_not_the_type_name() {
        // 插件乙用 `b_flag` 当标记，不是自己的 id：只看 id 的旧实现会漏掉它。
        let p = params(json!({"type": "sftp", "b_flag": true}));
        assert_eq!(owner_in(&table(), &p).plugin_id(), Some("beta"));
    }

    #[test]
    fn a_registered_remote_stays_an_orphan_while_its_plugin_is_gone() {
        // 登记还在、插件被停用：必须认得出这是插件远程，且不能声称能接管。
        let claims = registry(&[("pic", "alpha", "alpha")]);
        let only_beta = vec![table()[1].clone()];
        match resolve_in(&claims, &only_beta, "pic") {
            Owner::Orphan { marker } => assert_eq!(marker, "alpha"),
            other => panic!("不能退化成 Native：{other:?}"),
        }
        assert_eq!(
            resolve_in(&claims, &table(), "pic").plugin_id(),
            Some("alpha")
        );
    }

    #[test]
    fn a_plugin_that_changed_its_marker_leaves_the_remote_an_orphan() {
        // 新版把标记键改了名：旧登记对不上，也不能悄悄退回原生访问。
        let claims = registry(&[("pic", "alpha", "alpha")]);
        let mut renamed = table();
        renamed[0].marker = "alpha2".into();
        match resolve_in(&claims, &renamed, "pic") {
            Owner::Orphan { marker } => assert_eq!(marker, "alpha"),
            other => panic!("不能退化成 Native：{other:?}"),
        }
    }

    #[test]
    fn other_backends_and_custom_keys_are_never_claimed() {
        // 这几组是用户明确要求「普通远程不受影响」的部分：参数里有一堆宿主
        // 不认识的键（旧实现靠猜，会把它们判成孤儿并锁死）。
        for raw in [
            json!({"type": "s3", "provider": "AWS", "bucket": "b", "region": "eu-west-1", "endpoint": "", "access_key_id": "AK"}),
            json!({"type": "drive", "root_folder_id": "abc", "scope": "drive", "team_drive": ""}),
            json!({"type": "sftp", "host": "h", "user": "u", "key_file": "k"}),
            json!({"type": "webdav", "url": "https://example.org/dav", "vendor": "other",
                   "bearer_token": "abc", "no_check_certificate": true, "use_trash": false,
                   "webdav_vendor": "other", "headers": ["A: b"]}),
        ] {
            let p = params(raw.clone());
            assert_eq!(owner_in(&table(), &p), Owner::Native, "{raw} 不该被认领");
        }
        // 运行期更彻底：没登记就是普通远程，连参数都不读。
        let claims = BTreeMap::new();
        for name in ["s3box", "gdrive", "vps", "dav"] {
            assert_eq!(resolve_in(&claims, &table(), name), Owner::Native);
        }
    }

    #[test]
    fn off_values_do_not_count_as_a_marker() {
        for v in [json!("0"), json!(false), json!(""), json!(null)] {
            let p = params(json!({"type": "webdav", "alpha": v}));
            assert_eq!(owner_in(&table(), &p), Owner::Native, "{v} 不该算标记");
        }
    }

    #[test]
    fn an_undeclared_op_is_not_taken() {
        let owner = Owner::Plugin {
            id: "alpha".into(),
            name: "插件甲".into(),
            ops: vec![LIST.into()],
        };
        assert!(owner.takes(LIST));
        assert!(!owner.takes(UPLOAD));
    }

    #[test]
    fn entries_are_read_from_either_shape() {
        let a = parse_entries(
            &json!([{"name": "a.txt", "size": 3, "uploaded": "2026-01-02T03:04:05Z"}]),
        )
        .expect("数组");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "a.txt");
        assert!(!a[0].is_dir);

        let b = parse_entries(&json!({"items": [{"name": "d", "is_dir": true}]})).expect("对象");
        assert!(b[0].is_dir && b[0].size == 0);
    }

    #[test]
    fn entries_without_a_name_are_dropped() {
        let v = json!([{"name": ""}, {"is_dir": true}, {"name": "ok"}]);
        assert_eq!(parse_entries(&v).expect("ok").len(), 1);
    }

    #[test]
    fn the_upload_report_counts_what_the_user_cares_about() {
        let up = Uploaded {
            total: 3,
            ok: 3,
            single: false,
            dedup: 1,
            ..Default::default()
        };
        let (level, msg) = up.report("相册");
        assert_eq!(level, 1);
        assert!(msg.contains("3") && msg.contains("相册"), "{msg}");
    }

    #[test]
    fn a_failed_upload_says_how_many_are_missing() {
        let up = Uploaded {
            total: 6,
            ok: 2,
            failed: (0..4).map(|i| format!("f{i}: 超时")).collect(),
            single: false,
            ..Default::default()
        };
        let (level, msg) = up.report("相册");
        assert_eq!(level, 2);
        assert!(msg.contains("2/6") && msg.contains("另有 1 条"), "{msg}");
    }

    #[test]
    fn orphan_messages_never_name_a_plugin() {
        // 提示只报远程名和标记键：认领者已经不在，报名字就是凭空猜。
        let m = orphan_msg("相册", "alpha");
        assert!(m.contains("相册") && m.contains("alpha"));
        assert!(!m.to_lowercase().contains("kvault"));
    }

    #[test]
    fn a_folder_dropped_for_upload_keeps_its_subdirectories() {
        // 摊平过一次：三层目录里的同名文件全落到同一层，后传的把先传的盖掉
        // （站点按内容去重，看起来像「少传了几个」）。所以层级必须带上去。
        let root = std::env::temp_dir().join(format!(
            "rcgui-expand-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let deep = root.join("sub").join("deeper");
        std::fs::create_dir_all(&deep).expect("建目录");
        std::fs::write(root.join("top.txt"), b"a").expect("写文件");
        std::fs::write(deep.join("leaf.txt"), b"b").expect("写文件");

        let got = expand(&[root.clone()]);
        let _ = std::fs::remove_dir_all(&root);

        let mut pairs: Vec<(String, String)> =
            got.into_iter().map(|(_, name, rel)| (rel, name)).collect();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("".to_string(), "top.txt".to_string()),
                ("sub/deeper".to_string(), "leaf.txt".to_string()),
            ]
        );
    }

    #[test]
    fn the_target_directory_never_gets_a_stray_slash() {
        assert_eq!(join_dir("", ""), "");
        assert_eq!(join_dir("/相册/", ""), "相册");
        assert_eq!(join_dir("相册", "sub/"), "相册/sub");
        assert_eq!(join_dir("", "/sub/"), "sub");
    }

    /// 插件装上、停用、重装、升级一轮走下来，归属只跟着「现在有没有认领者」变。
    ///
    /// 这些断言是给 `remote.list`/`remote.upload`/`remote.delete` 的口子用的：
    /// 停用之后必须变成孤儿（拒绝），不能掉回原生访问——原生访问会用完全不同的
    /// 协议去碰同一批文件。登记表是这一路唯一的依据，所以这里喂的是登记表，
    /// 不是远程参数。
    #[test]
    fn a_remote_follows_its_plugin_through_install_disable_reinstall_and_upgrade() {
        let claims = registry(&[("pic", "alpha", "alpha")]);

        let installed = table();
        let owner = resolve_in(&claims, &installed, "pic");
        assert_eq!(owner.plugin_id(), Some("alpha"));
        assert!(owner.claimed() && owner.takes(LIST) && owner.takes(DELETE));

        // 停用：清单还在磁盘上，但从「已启用的插件」列表里消失了。
        let disabled = vec![table()[1].clone()];
        let owner = resolve_in(&claims, &disabled, "pic");
        assert_eq!(owner.plugin_id(), None);
        assert!(owner.claimed(), "登记还在，就得认出这是插件远程");
        assert!(!owner.takes(LIST), "认领者不在时不能声称能接管");

        // 重装：同一个插件回来，声明照旧。
        let owner = resolve_in(&claims, &table(), "pic");
        assert_eq!(owner.plugin_id(), Some("alpha"));
        assert!(owner.takes(LIST));

        // 升级：新版把删除从声明里拿掉了，宿主必须跟着不再转发删除。
        let mut upgraded = table();
        upgraded[0].ops = vec![LIST.into()];
        let owner = resolve_in(&claims, &upgraded, "pic");
        assert!(owner.takes(LIST));
        assert!(!owner.takes(DELETE), "新版没声明删除，就不能删");
    }

    /// 卸载之后（登记表还在、插件没了）重启也不会变回普通远程。
    ///
    /// 这一条是「重启」与「卸载」两件事的交点：`resolve_in` 只读文件与插件表，
    /// 进程重来一遍拿到的还是 `Orphan`，不会因为内存里没有上次的状态就放手。
    #[test]
    fn a_claim_survives_a_restart_with_the_plugin_uninstalled() {
        let dir = std::env::temp_dir().join(format!(
            "rg-owners-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let file = dir.join(crate::remote_owners::FILE);
        let claims = registry(&[("pic", "alpha", "alpha")]);
        crate::remote_owners::save_to(&file, &claims).expect("写登记表");
        let reloaded = crate::remote_owners::load_from(&file).expect("读登记表");
        let _ = std::fs::remove_dir_all(&dir);

        match resolve_in(&reloaded, &[], "pic") {
            Owner::Orphan { marker } => assert_eq!(marker, "alpha"),
            other => panic!("重启后不能退化成 Native：{other:?}"),
        }
    }

    /// 普通 WebDAV 远程与插件的存在与否无关：装了什么插件都不能认领它。
    #[test]
    fn a_plain_webdav_remote_stays_native_with_no_plugins_at_all() {
        let p = params(json!({
            "type": "webdav", "url": "https://example.org/dav", "vendor": "other",
            "user": "u", "pass": "p",
        }));
        assert_eq!(owner_in(&[], &p), Owner::Native);
        assert_eq!(owner_in(&table(), &p), Owner::Native);
        // 运行期连参数都不看：没登记就是普通远程。
        let claims = BTreeMap::new();
        assert_eq!(resolve_in(&claims, &[], "dav"), Owner::Native);
        assert_eq!(resolve_in(&claims, &table(), "dav"), Owner::Native);
    }

    // ---- `rclone.upload_file`：字节该由谁发、发到哪儿 ----

    fn uploader(id: &str, name: &str, ops: &[&str]) -> Owner {
        Owner::Plugin {
            id: id.into(),
            name: name.into(),
            ops: ops.iter().map(|o| (*o).to_string()).collect(),
        }
    }

    #[test]
    fn a_windows_drive_is_not_a_remote_name() {
        // `C:\imgs\a.png` 里冒号前只有盘符。认成远程名的话，一次往本机目录的普通上传
        // 会跑去问插件要授权，用户看到的提示是「远程 C 由插件提供」。
        assert_eq!(remote_name_of(r"C:\imgs\a.png"), None);
        assert_eq!(remote_name_of("D:/imgs/a.png"), None);
        assert_eq!(remote_name_of("C:/"), None);
        // 真的远程名照常认。
        assert_eq!(remote_name_of("kvault:/blog/a.png"), Some("kvault"));
        assert_eq!(remote_name_of("kvault:"), Some("kvault"));
        // 冒号前面带路径分隔符的，不是远程名。
        assert_eq!(remote_name_of("./a:b.png"), None);
        assert_eq!(remote_name_of(":a.png"), None);
    }

    #[test]
    fn a_native_remote_keeps_the_old_path() {
        // 没有插件认领就得返回 `None`：调用方照旧 copyfile，普通远程的上传行为不变。
        assert_eq!(pick_uploader(&Owner::Native, "pic"), Ok(None));
    }

    #[test]
    fn a_missing_plugin_fails_instead_of_falling_back() {
        // 插件停用、远程还在：这时候退回原生 copyfile 就是拿 WebDAV 的 PUT 去写一个
        // 只认自己接口的站点，用户看到的只会是那条莫名其妙的 403。必须明说。
        let owner = Owner::Orphan {
            marker: "kvault".into(),
        };
        let msg = pick_uploader(&owner, "kvault").expect_err("不能退回原生访问");
        assert!(msg.contains("没有启用"), "{msg}");
        assert!(msg.contains("kvault"), "{msg}");
    }

    #[test]
    fn a_plugin_that_does_not_take_uploads_says_so() {
        // 插件甲只声明了 list 与 delete：主程序不能替它猜怎么收文件。
        let owner = owner_in(&table(), &params(json!({"type": "webdav", "alpha": "1"})));
        let msg = pick_uploader(&owner, "pic").expect_err("没声明就不接管");
        assert!(msg.contains("插件甲"), "{msg}");
        assert!(msg.contains(UPLOAD), "{msg}");
    }

    #[test]
    fn the_plugin_that_declares_uploads_gets_the_file() {
        let owner = uploader("kvault", "K-Vault", &[LIST, UPLOAD]);
        assert_eq!(
            pick_uploader(&owner, "kvault"),
            Ok(Some("kvault".to_string()))
        );
    }

    #[test]
    fn the_target_matches_what_a_native_copy_would_write() {
        // 与 copyfile 同口径：`remote_path` 相对 `fs` 的路径部分。换条路走插件时位置
        // 不能变，否则同一个请求在两种远程上会落到不同的地方。
        assert_eq!(
            target_of("kvault:/blog", "2024/a.png", "a.png"),
            ("blog/2024".to_string(), "a.png".to_string())
        );
        assert_eq!(
            target_of("kvault:", "a.png", "a.png"),
            (String::new(), "a.png".to_string())
        );
        // 只给了文件名：目标不能退化成空路径。
        assert_eq!(
            target_of("kvault:/blog", "", "IMG_0001.png"),
            ("blog".to_string(), "IMG_0001.png".to_string())
        );
        // 两侧多余的斜杠不能叠出空层。
        assert_eq!(
            target_of("kvault:/", "/a.png", "a.png"),
            (String::new(), "a.png".to_string())
        );
    }

    #[test]
    fn a_single_letter_remote_is_still_asked_who_owns_it() {
        // `x:` 两头都可能：盘符相对路径，或者真有一条叫 `x` 的远程。以前一律当盘符，
        // 于是一条合法的插件远程 `x:` 直接滑进原生写入——那就是拿 WebDAV 的 PUT 去写
        // 别人家的接口。
        assert_eq!(remote_name_of("x:"), Some("x"));
        assert_eq!(remote_name_of("x:album/a.png"), Some("x"));
        // 有人认领就走插件。
        let owner = uploader("x", "某插件", &[LIST, UPLOAD]);
        assert_eq!(pick_uploader(&owner, "x"), Ok(Some("x".to_string())));
        // 没人认领还是原生，普通上传的行为没变。
        assert_eq!(pick_uploader(&Owner::Native, "x"), Ok(None));
        // 盘符后面紧跟分隔符才是板上钉钉的本机路径。
        assert_eq!(remote_name_of(r"x:\album\a.png"), None);
        assert_eq!(remote_name_of("x:/album"), None);
    }

    #[test]
    fn every_upload_holds_its_own_grant_key() {
        let a = next_grant_key();
        let b = next_grant_key();
        assert_ne!(a, b, "两条上传不能共用一个授权键");
        // 键要过得了 `plugin_host::upload` 的 `is_key`：ASCII，长度不超过 64。
        for key in [&a, &b] {
            assert!(key.len() <= 64, "{key}");
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
                "{key}"
            );
        }
    }

    #[test]
    fn concurrent_uploads_never_share_a_grant_key() {
        // 并发上传靠这个号区分授权。撞一次，插件回来取字节拿到的就是别人的文件，
        // 撤销时还会把别人那条一起撤掉。
        let seen = std::sync::Mutex::new(std::collections::HashSet::new());
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..64 {
                        let key = next_grant_key();
                        assert!(seen.lock().unwrap().insert(key), "授权键撞了");
                    }
                });
            }
        });
        assert_eq!(seen.into_inner().unwrap().len(), 8 * 64);
    }

    #[test]
    fn a_finished_upload_releases_only_its_own_authorisation() {
        let dir =
            std::env::temp_dir().join(format!("rg-grants-{}-{}", std::process::id(), grant_key(0)));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let first = dir.join("first.png");
        let second = dir.join("second.png");
        std::fs::write(&first, b"first").expect("写 first");
        std::fs::write(&second, b"second").expect("写 second");

        let plugin = "rg-grant-scope-test";
        let key_a = next_grant_key();
        let key_b = next_grant_key();
        plugin_host::upload::grant(plugin, &key_a, &first).expect("登记 first");
        plugin_host::upload::grant(plugin, &key_b, &second).expect("登记 second");
        // 键不同，两条授权才不会互相顶掉。
        assert!(plugin_host::upload::waiting(plugin, &key_a));
        assert!(plugin_host::upload::waiting(plugin, &key_b));

        {
            let _guard = GrantGuard {
                plugin: plugin.to_string(),
                key: key_a.clone(),
            };
        }
        // 一条走完只撤自己那条；另一条还在等它自己的发送，不能被连带撤掉。
        assert!(
            !plugin_host::upload::waiting(plugin, &key_a),
            "走完的授权该撤掉"
        );
        assert!(
            plugin_host::upload::waiting(plugin, &key_b),
            "别撤别人的授权"
        );

        plugin_host::upload::revoke(plugin, &key_b);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
