//! 应用配置：数据目录解析 + config.json 读写（原子写）
//! 便携模式：exe 旁存在 "portable" 文件时数据目录 = exe目录\data

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 连接用的 RC 配置：连到哪个地址、用什么账号口令、启动时要不要自动连。
///
/// 这里只描述「连接」这一件事，进程由谁管是 [`CoreCfg`] 的 `mode`：managed 时
/// 程序自己挑空闲端口、生成一次性口令、起子进程并握手；external 时程序只连，
/// 不启动、不停止、不重启对方。所以 managed 那次握手的临时凭据属于运行时状态，
/// 不写回这里——写回来就等于把一次性口令变成用户下次启动时的连接凭据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RcloneCfg {
    /// 完整 RC endpoint，如 `http://127.0.0.1:5572`
    #[serde(default = "d_rc_url")]
    pub url: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
    /// 启动时自动连接（只是连 API，不启动任何进程）
    #[serde(default = "d_true")]
    pub connect_on_start: bool,
}

fn d_rc_url() -> String {
    "http://127.0.0.1:5572".into()
}

impl Default for RcloneCfg {
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_http_port")]
    pub port: u16,
}
fn d_true() -> bool {
    true
}
fn d_http_port() -> u16 {
    5273
}
impl Default for HttpCfg {
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }
}

/// 自动挂载项（配置跟随远程：远程被删除时级联删除）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoMount {
    /// 远程名（不含冒号，如 "teldrive"）
    pub remote: String,
    /// 远程内子目录（相对路径；空串 = 远程根目录）
    #[serde(default)]
    pub sub_path: String,
    pub point: String,
    #[serde(default)]
    pub ro: bool,
    #[serde(default = "d_cache")]
    pub cache: String,
    /// 是否启用：停用后启动软件时不自动挂载（配置保留）
    #[serde(default = "d_true")]
    pub enabled: bool,
}
fn d_cache() -> String {
    "writes".into()
}

/// 自动服务项（配置跟随远程：远程被删除时级联删除）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoServe {
    pub stype: String,
    /// 远程名（不含冒号）
    pub remote: String,
    /// 远程内子目录（空串 = 远程根目录）
    #[serde(default)]
    pub sub_path: String,
    pub addr: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
    /// 是否启用：停用后启动软件时不自动启动
    #[serde(default = "d_true")]
    pub enabled: bool,
}

/// 服务自定义名称映射（按 协议+远程+子路径+地址 匹配）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServeName {
    pub stype: String,
    pub remote: String,
    #[serde(default)]
    pub sub_path: String,
    pub addr: String,
    pub name: String,
}

fn d_unit() -> i32 {
    1
}

/// 由本程序管理的 rclone 内核。
///
/// `mode` 只有两个合法值：
/// - `"managed"`（默认）：程序自己挑空闲端口、自己生成 RC 账号口令、自己启动子进程并握手。
///   退出时只关自己启动的那个进程。
/// - `"external"`：用户已经有一个 rclone 在跑，程序只连接、不启动、不停止、不重启它。
///
/// `path` 是用户指定的内核可执行文件。查找顺序是 **`path` → 数据目录 `core/` 里
/// 用户明确装过的那份 → 程序同级 `core/` 与数据目录 `core/` 中版本较新的那个 →
/// `PATH`**：显式填的路径排在最前，因为官方 rclone 缺少某些 fork 才有的后端
/// （例如 `teldrive`），插件随包部署的 fork 一旦被同级目录里的官方 exe 盖住，
/// 用户设的路径就永远不生效——「设置存在但不生效」比没有设置更糟。
///
/// 第二档看 [`CoreCfg::has_managed_install`]：装过就稳定用装的那份，不再比版本。
/// 版本号比不出渠道（`v1.77.0-extra` 的数字比 `v1.76.0` 大，但用户要的是原版），
/// 而每次换便携包都重新比版本，等于让随包那份在用户不知情时把渠道改回去。
/// 一份都没装过（老配置、第一次运行）时才退回按版本比较，新者胜；两边都探测不出
/// （例如跑不起来）时选数据目录那份，好让下一次内核更新能覆盖它。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreCfg {
    #[serde(default = "d_managed")]
    pub mode: String,
    #[serde(default)]
    pub path: String,
    /// 启动时检查新版本。默认开，但检查在后台进行，不阻塞窗口显示。
    #[serde(default = "d_true")]
    pub update_check: bool,
    /// 上次成功握手过的内核版本，仅用于界面显示与「内核被换掉了」的提示。
    #[serde(default)]
    pub last_version: String,
    /// 下载/更新走哪个渠道：`"extra"`（增强版，默认）或 `"official"`（原版）。
    ///
    /// 存空串表示「这份配置还没选过」——此时按 [`CoreCfg::normalized_flavor`]
    /// 从 `last_version` 推断，这样老配置里已经装着官方版的用户不会被下一次更新
    /// 悄悄换成增强版；全新配置（`last_version` 也空）则落到默认的增强版。
    #[serde(default)]
    pub flavor: String,
    /// 以前手填过的内核路径，按先后顺序留着。
    ///
    /// 「改用托管内核」只把它记到这里、把 `path` 清空，**文件本身不动**：用户填路径
    /// 往往是为了某个后端特地挑的 fork，删掉或覆盖等于把他的选择扔掉。想换回去时
    /// 照着这里的路径重新填进「程序路径」即可。
    #[serde(default)]
    pub kept_paths: Vec<String>,
}

fn d_managed() -> String {
    "managed".to_string()
}

impl Default for CoreCfg {
    fn default() -> Self {
        Self {
            mode: d_managed(),
            path: String::new(),
            update_check: true,
            last_version: String::new(),
            flavor: String::new(),
            kept_paths: Vec::new(),
        }
    }
}

impl CoreCfg {
    /// 规范化 `mode`：只有 `"external"` 是另一个合法值，其余（含空串/旧配置）
    /// 一律回到 `"managed"`。
    pub fn normalized_mode(&self) -> &'static str {
        if self.mode.eq_ignore_ascii_case("external") {
            "external"
        } else {
            "managed"
        }
    }

    /// 实际生效的下载渠道，兼作旧配置迁移。
    ///
    /// 显式存了值就照它；没存过就从 `last_version` 反推（`-extra` 后缀是增强版，
    /// 只有数字是官方版），两者都没有才用默认的增强版。只看已握手过的版本而不看
    /// 目录里的 exe，是因为版本串是唯一稳定的信号，而 exe 可能被人换过。
    pub fn normalized_flavor(&self) -> crate::core::Flavor {
        if !self.flavor.trim().is_empty() {
            crate::core::Flavor::parse(&self.flavor)
        } else if !self.last_version.trim().is_empty() {
            crate::core::Flavor::of_version(&self.last_version)
        } else {
            crate::core::Flavor::Extra
        }
    }

    /// 上次装好后写下的渠道。没写过（老配置）返回 `None`，让调用方决定是去推还是重装。
    pub fn recorded_flavor(&self) -> Option<crate::core::Flavor> {
        if self.flavor.trim().is_empty() {
            None
        } else {
            Some(crate::core::Flavor::parse(&self.flavor))
        }
    }

    /// 数据目录 `core/` 里那份是不是用户明确装过/选过的托管内核。
    ///
    /// 判断依据只有 `flavor`（渠道）这一个字段，因为它的写入点只有三处：`--core-install`
    /// 装成功、维护页点渠道（`core-flavor`）、界面点「安装内核」装成功（`core_install`）。
    /// 三处都是"用户明确要这个渠道"，所以它非空就说明数据目录那份的来历清楚。
    ///
    /// 不拿 `last_version` 判断：那是握手回来的实际版本，可能来自随包的内核或别人的
    /// 实例，跟"我们装过没有"没关系；用它当凭据会让没装过的人也走托管首选。
    pub fn has_managed_install(&self) -> bool {
        self.recorded_flavor().is_some()
    }

    /// 把手填的内核路径收进备选、改回托管模式，返回被收起来的那个路径。
    ///
    /// 只改配置，不碰文件：这是「改用托管内核」按钮的全部动作，下载与替换留给
    /// `core-install`，于是用户能在同一个界面里分两步看清后果。
    pub fn adopt_managed(&mut self) -> Option<String> {
        let custom = self.path.trim().to_string();
        self.path.clear();
        self.mode = d_managed();
        if custom.is_empty() {
            return None;
        }
        if !self
            .kept_paths
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&custom))
        {
            self.kept_paths.push(custom.clone());
        }
        Some(custom)
    }
}

/// 一个同步文件夹任务。
///
/// 默认 `direction = "up"`（本地 → 远程）、`mirror = false`（只 copy，不删除目标多余文件）。
/// 镜像删除（`sync` 语义）必须由用户在本界面显式勾选，因为它的后果是"目标端多出来的文件被删掉"，
/// 一旦方向或路径选错就是数据丢失。双向同步不提供：rclone 的 bisync 需要额外的状态目录与
/// 冲突策略，默认打开它等于把"哪边更新"这种判断交给一个没看过配置的人。
///
/// `direction` / `mode` 的解释只有一处：`crate::sync_tasks`（`is_up` / `is_mirror` /
/// `pair` / `ui_direction`）。这里只存字符串，不要再加第二套语义。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncTask {
    /// 稳定 id（创建时生成），改名不影响调度
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// 本地目录绝对路径
    pub local: String,
    /// 远程路径，形如 `remote:sub/dir`
    pub remote: String,
    /// `"up"` | `"down"`
    #[serde(default = "d_up")]
    pub direction: String,
    /// `"copy"`（默认，不删目标）| `"sync"`（镜像，删除目标多余文件）
    #[serde(default = "d_copy")]
    pub mode: String,
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// 本机文件夹有变化就提前跑一轮，不必等到下一个间隔。
    ///
    /// `None` 表示用户没选过：只复制的任务盯，镜像任务不盯。镜像会删对端，一次改名事件
    /// 就可能删掉一大片，那得用户自己打开，不该由默认值决定。
    ///
    /// 只对「本地 → 远程」方向生效：反过来的时候，本机文件的变化正是用户自己的编辑，
    /// 拿它去触发同步会把编辑覆盖掉（见 `crate::sync_watch::watches`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_sync: Option<bool>,
    /// 间隔小时，0 表示只在启动时跑一次
    #[serde(default = "d_six")]
    pub interval_hours: u32,
    /// 上次运行开始时刻（RFC3339，空 = 从未运行）
    #[serde(default)]
    pub last_run: String,
    /// 上次运行结果：`""` | `"ok"` | `"failed: 原因"`
    #[serde(default)]
    pub last_status: String,
}

fn d_up() -> String {
    "up".to_string()
}

fn d_copy() -> String {
    "copy".to_string()
}

fn d_six() -> u32 {
    6
}

/// 轻量模式：窗口隐藏到托盘一段时间后释放界面占用的内存。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// 隐藏多少分钟后进入轻量模式
    #[serde(default = "d_ten")]
    pub minutes: u32,
}

fn d_ten() -> u32 {
    // 和轻量模式自己用的默认值保持同一个来源，免得两处慢慢走散
    crate::lightweight::DEFAULT_MINUTES
}

impl Default for LightCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            minutes: crate::lightweight::DEFAULT_MINUTES,
        }
    }
}

/// WebDAV 备份目标（导出/导入 rcgui.conf 用）。
///
/// `pass` 与 URL 里的 userinfo 都算凭据：导出到文件时默认剔除，
/// 只有用户显式勾选"包含凭据"才写进去。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupCfg {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pass: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub rclone: RcloneCfg,
    /// 0 跟随系统 / 1 浅色 / 2 深色
    #[serde(default)]
    pub theme_mode: u8,
    /// 强调色：0 蓝 / 1 绿 / 2 紫 / 3 橙
    #[serde(default)]
    pub accent_idx: u8,
    /// 界面语言：`"system"` 跟随系统 / `"zh"` 简体中文 / `"en"` English。
    ///
    /// 空串（老配置没有这个字段）与 `"system"` 等价。
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub silent_start: bool,
    /// 关闭窗口时隐藏到托盘（而不是退出）。
    ///
    /// 首次关闭时由弹层询问用户，用户的选择写进这里；之后按此值执行。
    #[serde(default)]
    pub close_to_tray: bool,
    /// 用户是否已经就「关闭行为」做过选择（确认 / 取消 / 直接关闭，或手动改过复选框）。
    ///
    /// 为 true 时不再弹首次提示框。注意它与 `close_to_tray` 是两件事：
    /// 用户可能选过「直接关闭」（`close_to_tray=false`），此时也不该再问。
    #[serde(default)]
    pub close_prompt_done: bool,
    #[serde(default)]
    pub http: HttpCfg,
    /// 插件启用表
    #[serde(default)]
    pub plugin_enabled: std::collections::HashMap<String, bool>,
    /// 用户卸载过的内置插件 id（墓碑）。
    ///
    /// 内置插件与用户插件在磁盘上同构，卸载后目录就没了；没有墓碑的话下次启动
    /// 又会被 `plugins::ensure_builtin` 释放回来，用户会觉得「删不掉」。
    /// 点「恢复内置」时从这张表里移除。
    #[serde(default)]
    pub plugin_removed: Vec<String>,
    /// 自动挂载列表
    ///
    /// **不再写入 config.json**：唯一落盘位置是档案文档
    /// `<数据目录>/profiles/default.json`（见 `profiles.rs`）。这里保留字段是为了
    /// 让既有 UI 代码继续可用，同时仍能读取旧版 config.json 里的同名字段。
    #[serde(default, skip_serializing)]
    pub auto_mounts: Vec<AutoMount>,
    /// 自动服务列表（同上：只从档案文档落盘）
    #[serde(default, skip_serializing)]
    pub auto_serves: Vec<AutoServe>,
    /// 远程路径历史（最近 10 条，最新在前）
    #[serde(default)]
    pub path_history: Vec<String>,
    /// 服务自定义名称映射（同上：只从档案文档落盘）
    #[serde(default, skip_serializing)]
    pub serve_names: Vec<ServeName>,
    /// 代理配置表（同上：只从档案文档落盘）
    #[serde(default, skip_serializing)]
    pub proxies: Vec<rcgui_profile::ProxyProfile>,
    /// remote → 代理绑定（同上）
    #[serde(default, skip_serializing)]
    pub proxy_bindings: Vec<rcgui_profile::RemoteProxyBinding>,
    /// 全局上传限速（数值文本，空 = 不限；rclone 仅支持全局 tx:rx 限速）
    #[serde(default)]
    pub bw_up: String,
    #[serde(default = "d_unit")]
    pub bw_up_unit: i32,
    /// 全局下载限速（数值文本，空 = 不限）
    #[serde(default)]
    pub bw_down: String,
    #[serde(default = "d_unit")]
    pub bw_down_unit: i32,
    /// winget 镜像源名（只存名字，源地址由用户自己 `winget source add` 注册）。
    ///
    /// 挂载页「一键安装 WinFsp」选镜像源时，命令里带上 `--source <这个名字>`；
    /// 留空则镜像源不可用。这里刻意不内置任何镜像地址：程序不该替用户改 winget 配置，
    /// 也不该在代码里固化第三方源。
    #[serde(default)]
    pub winget_mirror_source: String,
    /// 由本程序管理的 rclone 内核（见 [`CoreCfg`]）
    #[serde(default)]
    pub core: CoreCfg,
    /// 同步文件夹任务
    #[serde(default)]
    pub sync_tasks: Vec<SyncTask>,
    /// 轻量模式
    #[serde(default)]
    pub light: LightCfg,
    /// WebDAV 备份目标
    #[serde(default)]
    pub backup: BackupCfg,
    /// 是否已把旧 oneclick 插件的设置迁移到 [`CoreCfg`]。
    ///
    /// 迁移只做一次：之后用户再改 `core.path` 不该被旧设置覆盖回去。
    #[serde(default)]
    pub oneclick_migrated: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }
}

impl AppConfig {
    /// 设置某 remote 的代理绑定（`proxy_id` 为空串 = 明确不代理）。
    ///
    /// 换目标时**只清 `applied`，保留脚印**：`applied_value` / `applied_key` /
    /// `original_override` 是回滚的唯一依据，清掉就等于“从未管理过”，
    /// 切回「不代理」时 apply 会判定无需动作，rclone 里的旧代理永远删不掉。
    /// 与 `ProfileDoc::bind_proxy` 的语义保持一致。
    pub fn bind_proxy_slot(&mut self, remote: &str, proxy_id: &str) {
        let key = remote.trim_end_matches(':');
        match self
            .proxy_bindings
            .iter_mut()
            .find(|b| b.remote.eq_ignore_ascii_case(key))
        {
            Some(b) => {
                if b.proxy_id != proxy_id {
                    b.proxy_id = proxy_id.to_string();
                    b.applied = false;
                }
            }
            None => self.proxy_bindings.push(rcgui_profile::RemoteProxyBinding {
                remote: key.to_string(),
                proxy_id: proxy_id.to_string(),
                ..Default::default()
            }),
        }
    }
}

/// 显式指定的数据目录（`RCGUI_DATA_DIR`），没有设置时返回 `None`。
///
/// 设了它就意味着「这次运行用的是别人的配置」：便携目录、`%APPDATA%\RCGUI` 都不再参与，
/// **也不做任何旧配置迁移**。复制一份真实用户数据进验收目录会同时污染两边——
/// 验收脚本会以为改的是自己的配置，用户那边则多出一份来历不明的记录。
///
/// 空白值按未设置处理（`RCGUI_DATA_DIR=` 这种写法在 PowerShell 里很常见）。
/// 相对路径按当前工作目录解析成绝对路径：相对目录会让「单实例隔离」「日志目录」
/// 跟着 cwd 漂移，是验收最难查的一类问题。
pub fn data_dir_override() -> Option<PathBuf> {
    override_dir(std::env::var("RCGUI_DATA_DIR").ok().as_deref())
}

/// [`data_dir_override`] 的纯函数版本。
///
/// 拆出来是为了能测：环境变量是进程级的，并行跑的测试改它会影响别的用例
/// （比如诊断测试会跟着写进别人的目录）。
fn override_dir(raw: Option<&str>) -> Option<PathBuf> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(raw);
    Some(if p.is_absolute() {
        p
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(p),
            Err(_) => p,
        }
    })
}

/// 应用数据根目录。
///
/// - `RCGUI_DATA_DIR` 有值：就是它（见 [`data_dir_override`]），不迁移、不猜。
/// - 便携模式（exe 旁边有 `portable` 文件）：`exe目录\data`
/// - 否则：`%APPDATA%\RCGUI`，并在首次启动时从旧目录 `%APPDATA%\rclone-gui` **复制**迁移。
///
/// 迁移原则（指南 §4）：复制而不是移动，旧目录**不删除、不修改**，随时可回滚；
/// 迁移失败则继续使用旧目录，绝不因为换目录导致用户数据「消失」。
pub fn data_dir() -> PathBuf {
    if let Some(dir) = data_dir_override() {
        // 验收目录通常事先不存在，这里建出来；权限自愈沿用同一套逻辑，
        // 但只对这一个目录生效，不碰 %APPDATA%。
        let _ = std::fs::create_dir_all(&dir);
        ensure_user_can_write_once(&dir);
        return dir;
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        if dir.join("portable").exists() {
            return dir.join("data");
        }
    }
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    // 展开可能的 8.3 短名（ADMINI~1 → Administrator），保证目录名与提示都正确
    let appdata = long_path(&PathBuf::from(appdata));
    let dir = resolve_data_dir(appdata.join("RCGUI"), appdata.join("rclone-gui"));
    // 确保本机用户对数据目录有写权限。
    //
    // 背景：如果程序曾以管理员身份运行过，它创建的目录 owner 会是
    // `BUILTIN\Administrators`，ACL 里往往只有 SYSTEM / Administrators / Local Account，
    // 没有普通用户或 Users 组的显式 ACE。用户之后以普通权限启动时，
    // 令牌里的 Administrators 是「仅用于拒绝」，于是任何写入都报
    // `拒绝访问 (os error 5)` —— 现象像配置错误，实则是目录权限。
    //
    // 只在「确实缺 Users 权限」时执行，避免每次取数据目录都去 spawn icacls。
    ensure_user_can_write_once(&dir);
    dir
}

/// 数据目录的隔离键：同一个目录的两个进程算出同一个值，不同目录算出不同的值。
///
/// 用途是单实例互斥量和唤醒通道的命名。它们必须跟着数据目录走：便携版、以及
/// 用 `RCGUI_DATA_DIR` 指到别处的验收实例，如果和用户正在运行的那个实例共用名字，
/// 第二个实例会直接退出并把窗口请求发给另一个配置的进程——表现为「点了没反应」，
/// 或者更糟：验收脚本操作的是用户真实数据的那份界面。
///
/// 先尝试规范化（消除 8.3 短名、`..`、符号链接），Windows 上再折叠大小写；
/// 规范化失败（目录还不存在）就用调用方给的路径，同一进程内仍然自洽。
pub fn data_dir_scope() -> String {
    scope_hash(&data_dir())
}

/// [`data_dir_scope`] 的前 16 个十六进制字符，够避免同机碰撞，也够短到能进名字。
fn scope_hash(dir: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut key = canon.display().to_string();
    while key.ends_with('\\') || key.ends_with('/') {
        key.pop();
    }
    if cfg!(windows) {
        key = key.to_lowercase();
    }
    let digest = Sha256::digest(key.as_bytes());
    let mut out = String::with_capacity(16);
    for b in &digest[..8] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 每个进程只尝试修复一次，且只在**确实缺少 Users 权限**时才真正执行。
///
/// 用 `OnceLock` 而不是 `Once`：需要记住结果供后续诊断。
static PERM_FIXED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn ensure_user_can_write_once(dir: &std::path::Path) {
    PERM_FIXED.get_or_init(|| {
        // 先做「真实可写性探测」，而不是只看 ACL 里有没有 Users。
        //
        // 只看 ACL 会误判：ACL 里可能有继承来的 `Users:(I)(F)` 条目，
        // 但文件 owner 是 `BUILTIN\Administrators`（管理员权限写过），
        // 普通权限进程在部分配置下仍然无法覆盖 —— 表现为「明明有权限却拒绝访问」。
        // 因此这里直接试着建一个临时文件，失败才去修。
        let writable = probe_writable(dir);
        debug_log(&format!(
            "[perm] dir={} writable={}",
            dir.display(),
            writable
        ));
        if writable {
            return true;
        }
        let ok = ensure_user_can_write(dir, true);
        let after = probe_writable(dir);
        debug_log(&format!(
            "[perm] repair attempted -> {ok}, writable_after={after}"
        ));
        ok && after
    });
}

/// 在当前目录真实建一个临时文件来判断可写性。
///
/// 这是最可靠的判断方式：ACL 看起来正确不代表真的能写（owner、完整性级别、
/// 继承标志都可能让写入失败），只有实际写一次才不会骗人。
fn probe_writable(dir: &std::path::Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".rcgui-wprobe-{}", std::process::id()));
    match std::fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 极简诊断日志：写到 `%APPDATA%\RCGUI\perm-debug.log`。
///
/// 仅用于排查「拒绝访问」这类难以复现的问题；写入失败完全忽略。
/// 注意**不能**调用 `data_dir()`（会递归进 `ensure_user_can_write_once`），
/// 所以这里自己拼路径。
fn debug_log(msg: &str) {
    if std::env::var("RCGUI_DEBUG_PERM").is_err() {
        return;
    }
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    let path = std::path::PathBuf::from(base)
        .join("RCGUI")
        .join("perm-debug.log");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{} {msg}", rcgui_profile::now_iso());
    }
}

/// 确保本目录对**普通用户**可写（补一条 `BUILTIN\Users` 的显式 ACE 并递归）。
///
/// 为什么授予 `Users` 而不是「当前用户名」：
/// - 目录 owner 可能是 `BUILTIN\Administrators`（管理员权限运行时创建）时，
///   非提升进程不能把 ACL 改成给「自己」授权，实测返回 ERROR_ACCESS_DENIED(5)；
/// - 给内置 `BUILTIN\Users` 组加 ACE 则是允许的，且覆盖本机所有交互用户。
///
/// ⚠️ 关键坑：**`icacls` 的退出码不可靠**。带 `/C` 时它「继续处理剩余文件」，
/// 即使某个路径报「拒绝访问」也仍然返回 0。因此必须检查输出里是否出现失败字样，
/// 不能只看 `status.success()`。这一条曾经让自愈逻辑「看起来成功、实际没生效」。
fn ensure_user_can_write(dir: &std::path::Path, quiet: bool) -> bool {
    if !dir.exists() {
        return false;
    }
    // 用绝对路径：GUI 进程的 PATH 不保证包含 System32
    let icacls = std::env::var("SystemRoot")
        .map(|r| format!(r"{r}\System32\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string());
    let out = std::process::Command::new(&icacls)
        .arg(dir)
        .arg("/grant")
        .arg("*S-1-5-32-545:(OI)(CI)F") // BUILTIN\Users，用 SID 避免本地化名称差异
        .arg("/T")
        .arg("/C")
        .arg("/Q")
        .creation_flags_no_window()
        .output();
    let _ = quiet;
    match out {
        Ok(o) => {
            if !o.status.success() {
                return false;
            }
            // exit code 为 0 不代表全部成功：扫输出里的失败信号
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            let combined = format!("{stdout}{stderr}");
            !(combined.contains(rcgui_i18n::t("拒绝访问"))
                || combined.contains("Access is denied")
                || combined.contains(rcgui_i18n::t("失败"))
                || combined.contains("Failed processing"))
        }
        Err(_) => false,
    }
}

/// 把 8.3 短名（如 `ADMINI~1`）解析成真实长路径。
///
/// `APPDATA` / `TEMP` 这类环境变量在某些会话里是短名形式，拼出来的路径
/// 人类看不懂，传给资源管理器也可能打不开。`canonicalize` 会展开短名。
fn long_path(p: &std::path::Path) -> PathBuf {
    match std::fs::canonicalize(p) {
        Ok(c) => {
            let s = c.to_string_lossy().to_string();
            PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
        }
        Err(_) => p.to_path_buf(),
    }
}
/// 选择数据目录，必要时执行一次性迁移。
fn resolve_data_dir(new_dir: PathBuf, legacy_dir: PathBuf) -> PathBuf {
    // 新目录已存在（已迁移过/全新安装）→ 直接用
    if new_dir.exists() || !legacy_dir.exists() {
        return new_dir;
    }
    match copy_tree(&legacy_dir, &new_dir) {
        Ok((files, bytes, skipped)) => {
            let marker = serde_json::json!({
                "schema_version": 1,
                "from": legacy_dir.display().to_string(),
                "to": new_dir.display().to_string(),
                "migrated_at": rcgui_profile::now_iso(),
                "files": files,
                "bytes": bytes,
                "skipped": skipped,
                // 回滚 = 删除新目录即可，旧目录从未被修改
                "rollback": rcgui_i18n::t("删除该 RCGUI 目录后重启，程序会回到旧目录 %APPDATA%\\rclone-gui")
            });
            let _ = write_atomic(
                &new_dir.join("migration.json"),
                marker.to_string().as_bytes(),
            );
            let mut msg = rcgui_i18n::f(
                "数据已从旧目录迁移到 {}（{} 个文件 / {} MB；旧目录保留，可回滚）",
                &[
                    &(new_dir.display()),
                    &(files),
                    &(format!("{:.1}", bytes as f64 / 1024.0 / 1024.0)),
                ],
            );
            if !skipped.is_empty() {
                msg.push_str(&rcgui_i18n::f(
                    "；{} 个条目被跳过（详见 migration.json）",
                    &[&(skipped.len())],
                ));
            }
            crate::profiles::push_notice(msg);
            new_dir
        }
        Err(e) => {
            // 只清理本次自己创建的不完整副本（进入本函数时 new_dir 尚不存在）
            let _ = std::fs::remove_dir_all(&new_dir);
            crate::profiles::push_notice(rcgui_i18n::f(
                "数据目录迁移失败（{}），本次继续使用旧目录 {}",
                &[&(e), &(legacy_dir.display())],
            ));
            legacy_dir
        }
    }
}

/// 递归复制目录树（跳过符号链接等特殊条目）。
///
/// **尽力而为**：单个文件失败（典型情况是 `rcd.log` 仍被运行中的 rclone 占用）
/// 只记为 skipped，不中断整个迁移——否则用户只要开着旧版 rclone，数据目录就永远迁不过去。
/// 返回 (文件数, 字节数, 跳过的条目)。
fn copy_tree(src: &Path, dst: &Path) -> Result<(u64, u64, Vec<String>)> {
    fn walk(src: &Path, dst: &Path, files: &mut u64, bytes: &mut u64, skipped: &mut Vec<String>) {
        if let Err(e) = std::fs::create_dir_all(dst) {
            skipped.push(format!("{}（{e}）", dst.display()));
            return;
        }
        let rd = match std::fs::read_dir(src) {
            Ok(rd) => rd,
            Err(e) => {
                skipped.push(format!("{}（{e}）", src.display()));
                return;
            }
        };
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    skipped.push(rcgui_i18n::f(
                        "{} 内部条目（{}）",
                        &[&(src.display()), &(e)],
                    ));
                    continue;
                }
            };
            let path = entry.path();
            let to = dst.join(entry.file_name());
            // 用 symlink_metadata 判定类型：不跟随符号链接，避免复制目标逃出数据目录
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    skipped.push(format!("{}（{e}）", path.display()));
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                skipped.push(rcgui_i18n::f("{}（符号链接）", &[&(path.display())]));
            } else if meta.is_dir() {
                walk(&path, &to, files, bytes, skipped);
            } else if meta.is_file() {
                match std::fs::copy(&path, &to) {
                    Ok(n) => {
                        *files += 1;
                        *bytes += n;
                    }
                    Err(e) => skipped.push(format!("{}（{e}）", path.display())),
                }
            } else {
                skipped.push(rcgui_i18n::f("{}（特殊文件）", &[&(path.display())]));
            }
        }
    }
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut skipped: Vec<String> = Vec::new();
    // 只有「连根目录都建不出来」才算致命错误
    std::fs::create_dir_all(dst).map_err(|e| {
        anyhow::anyhow!(
            "{}",
            rcgui_i18n::f("创建 {} 失败: {}", &[&(dst.display()), &(e)])
        )
    })?;
    walk(src, dst, &mut files, &mut bytes, &mut skipped);
    Ok((files, bytes, skipped))
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// 写入失败时的自愈：给目录补上「当前用户」的完全控制权限。
///
/// **为什么需要这个**：如果程序曾经以「管理员身份」运行过，它创建的
/// `%APPDATA%\RCGUI` 及其子目录会带上这样一份 ACL：
///
/// ```text
/// O:BAG:S-1-5-21-...-513D:(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)(A;OICIID;FA;;;LA)
/// ```
///
/// 只有 `SY`（SYSTEM）、`BA`（Administrators）、`LA`（Local Account）三条，
/// **没有 `Users` / 当前用户的显式 ACE**。而普通（非提升）进程的令牌里，
/// `BUILTIN\Administrators` 是「仅用于拒绝」(deny only)，`LA` 也不包含
/// 本机交互用户 —— 结果就是**普通权限下完全无法写入**，报
/// `拒绝访问 (os error 5)`。
///
/// 用户视角的现象极难诊断：配置明明没问题、文件也在，就是存不进去；
/// 而且往往是「以管理员跑过一次之后」才开始坏。
///
/// 这里用 `icacls` 给当前用户补 ACE（继承到子项），成功后原操作重试一次。
/// 用 `icacls` 而不是自己拼 SDDL：ACL 编辑涉及精确的 ACE 顺序与传播标志，
/// 交给系统工具更可靠，也便于用户用同样命令手动修复。
pub fn repair_data_dir_permissions() -> Result<(), String> {
    let dir = data_dir();
    if !dir.exists() {
        return Err(rcgui_i18n::f("数据目录不存在：{}", &[&(dir.display())]));
    }
    if ensure_user_can_write(&dir, false) {
        Ok(())
    } else {
        let account = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
            (Ok(d), Ok(u)) if !d.is_empty() && !u.is_empty() => format!("{d}\\{u}"),
            (_, Ok(u)) => u,
            _ => rcgui_i18n::t("<你的用户名>").to_string(),
        };
        Err(rcgui_i18n::f(
            "自动修复数据目录权限失败。可手动执行：icacls \"{}\" /grant \"{}:(OI)(CI)F\" /T",
            &[&(dir.display()), &(account)],
        ))
    }
}

/// 给 `std::process::Command` 加「不弹黑框」的 Windows 扩展。
trait NoWindowExt {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

impl NoWindowExt for std::process::Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW = 0x08000000，避免弹一下控制台
            self.creation_flags(0x08000000);
        }
        self
    }
}

/// 写入失败时统一的自愈包装：失败 → 尝试修权限 → 重试一次。
///
/// 只在「权限类错误」时才修，避免任何写失败都去动 ACL（那会让真正的
/// 磁盘满、文件被占用等问题更难诊断）。
pub fn write_atomic(file: &PathBuf, data: &[u8]) -> Result<()> {
    match write_atomic_once(file, data) {
        Ok(()) => Ok(()),
        Err(e) if is_permission_error(&e) => {
            repair_data_dir_permissions().map_err(|re| {
                anyhow::anyhow!(
                    "{}",
                    rcgui_i18n::f("{}；自动修复权限也失败了：{}", &[&(e), &(re)])
                )
            })?;
            // 修完重试一次；这次仍失败就把原始错误抛出去
            write_atomic_once(file, data)
        }
        Err(e) => Err(e),
    }
}

fn write_atomic_once(file: &PathBuf, data: &[u8]) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

/// 判断错误是否是「权限/拒绝访问」类。
fn is_permission_error(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::PermissionDenied {
                return true;
            }
            // Windows: ERROR_ACCESS_DENIED = 5
            if io.raw_os_error() == Some(5) {
                return true;
            }
        }
    }
    false
}

pub fn load() -> AppConfig {
    let mut cfg = load_raw();
    // 档案文档是「挂载 / 服务 / 预设名」的权威来源。
    // 档案不存在时，profiles 模块会把 config.json 里的旧字段迁移过去并落盘。
    if let Some(doc) = crate::profiles::load_doc(&cfg) {
        crate::profiles::apply_doc(&doc, &mut cfg);
    }
    cfg
}

/// 只读 config.json（应用偏好）。旧版本里可能仍带有挂载/服务字段，由调用方用档案覆盖。
fn load_raw() -> AppConfig {
    let p = config_path();
    if let Ok(raw) = std::fs::read(&p) {
        // 容忍 UTF-8 BOM（外部工具编辑配置文件时可能引入）
        let raw: &[u8] = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
            &raw[3..]
        } else {
            &raw
        };
        if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(raw) {
            migrate(&mut v);
            if let Ok(cfg) = serde_json::from_value::<AppConfig>(v) {
                return cfg;
            }
        }
    }
    AppConfig::default()
}

/// 旧格式迁移：auto_mounts/auto_serves/serve_names 中的
/// `fs: "name:sub/path"` 拆为 `remote: "name"` + `sub_path: "sub/path"`
fn migrate(v: &mut serde_json::Value) {
    migrate_rclone(v);
    /// "name:sub" → ("name", "sub")；"name:" → ("name", "")
    fn split_fs(fs: &str) -> (String, String) {
        match fs.find(':') {
            Some(i) => (
                fs[..i].trim().to_string(),
                fs[i + 1..].trim_matches('/').trim().to_string(),
            ),
            None => (fs.trim_end_matches(':').trim().to_string(), String::new()),
        }
    }

    let keys = ["auto_mounts", "auto_serves", "serve_names"];
    for key in keys {
        let Some(arr) = v.get_mut(key).and_then(|x| x.as_array_mut()) else {
            continue;
        };
        for it in arr.iter_mut() {
            // 仅迁移仍是旧格式（有 fs 无 remote）的条目
            if it.get("remote").is_none() {
                if let Some(fs) = it.get("fs").and_then(|x| x.as_str()) {
                    let (remote, sub) = split_fs(fs);
                    if let Some(obj) = it.as_object_mut() {
                        obj.insert("remote".into(), serde_json::json!(remote));
                        obj.insert("sub_path".into(), serde_json::json!(sub));
                        obj.remove("fs");
                    }
                }
            }
        }
    }
}

/// 旧 `rclone.mode/port/exe` → 新连接模型（`url` + `connect_on_start`）。
///
/// 迁移规则（按重构任务要求）：
/// - 旧 `mode`（0 托管 / 1 连接已有实例）**一律降级为「连接」语义**：不再自动启动进程；
/// - 旧托管模式本来就只认 `127.0.0.1:<port>`，因此用 `port` 拼出 endpoint；
/// - 旧连接模式保留用户填的 `url`；
/// - `connect_on_start` 默认 true：启动时尝试连接这个 endpoint，连不上就进离线模式。
fn migrate_rclone(v: &mut serde_json::Value) {
    let Some(rc) = v.get_mut("rclone").and_then(|x| x.as_object_mut()) else {
        return;
    };
    let had_mode = rc.contains_key("mode");
    let had_port = rc.get("port").and_then(|p| p.as_u64());
    let had_exe = rc.contains_key("exe");
    // 已经是新格式（没有旧字段）→ 不做任何改动，避免把默认值写坏
    if !had_mode && had_port.is_none() && !had_exe {
        return;
    }
    let mode = rc.get("mode").and_then(|m| m.as_u64()).unwrap_or(1);
    let url = rc
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let fallback = d_rc_url();
    let port_url: Option<String> = had_port
        .filter(|p| *p > 0)
        .map(|p| format!("http://127.0.0.1:{p}"));
    let mut new_url = url.clone();
    if mode == 0 {
        // 旧托管模式：原先连的就是 127.0.0.1:<port>，优先用端口拼
        if let Some(p) = port_url.clone() {
            new_url = p;
        } else if new_url.is_empty() {
            new_url = fallback.clone();
        }
    } else if new_url.is_empty() {
        new_url = port_url.unwrap_or(fallback);
    }
    if let Some(obj) = v.get_mut("rclone").and_then(|x| x.as_object_mut()) {
        obj.insert("url".into(), serde_json::json!(new_url));
        obj.entry("connect_on_start")
            .or_insert(serde_json::json!(true));
        // 旧字段清除：不再有「托管模式」和「自带 rclone.exe」这两个概念
        obj.remove("mode");
        obj.remove("port");
        obj.remove("exe");
        obj.remove("exe_custom");
    }
}

pub fn save(cfg: &AppConfig) -> Result<()> {
    // 顺序很重要：档案先写，失败就整体放弃。
    // 因为 config.json 已不再包含挂载/服务字段（skip_serializing），若先写 config.json
    // 而档案写失败，这些预设就只剩内存里一份，重启即丢。
    crate::profiles::save_from_cfg(cfg)?;
    // 第二步：应用偏好（已不含挂载/服务列表）
    let data = serde_json::to_vec_pretty(cfg)?;
    write_atomic(&config_path(), &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgui_profile::ProxyProfile;

    /// 没存过渠道的旧配置要按已装版本反推，别把官方版用户悄悄换成增强版。
    #[test]
    fn kernel_flavor_migrates_from_last_version() {
        use crate::core::Flavor;

        let mut c = CoreCfg::default();
        assert_eq!(c.flavor, "");
        // 全新配置：默认增强版。
        assert_eq!(c.normalized_flavor(), Flavor::Extra);

        // 装过官方版：反推成 official，切渠道之前不该改这个人的内核来源。
        c.last_version = "1.70.1".into();
        assert_eq!(c.normalized_flavor(), Flavor::Official);

        c.last_version = "1.76.0-extra".into();
        assert_eq!(c.normalized_flavor(), Flavor::Extra);

        // 显式选过就以它为准，哪怕和已装版本对不上（用户可能刚换完渠道还没下载）。
        c.flavor = "official".into();
        assert_eq!(c.normalized_flavor(), Flavor::Official);
        c.flavor = "extra".into();
        assert_eq!(c.normalized_flavor(), Flavor::Extra);

        // 存了空白等于没存：仍然回落到推断。
        c.flavor = "  ".into();
        assert_eq!(c.normalized_flavor(), Flavor::Extra);
    }

    /// 老配置文件里没有 `flavor` 这一项，读进来必须是"没选过"而不是被默认值写死。
    #[test]
    fn old_config_without_flavor_stays_unset() {
        let c: CoreCfg =
            serde_json::from_str(r#"{"mode":"managed","last_version":"1.70.1"}"#).unwrap();
        assert_eq!(c.flavor, "");
        assert_eq!(c.normalized_flavor(), crate::core::Flavor::Official);
    }

    /// 「改用托管内核」只搬配置：清掉 `path`、记进备选、回到托管模式，文件不动。
    #[test]
    fn adopt_managed_keeps_the_old_path_but_stops_using_it() {
        let mut c = CoreCfg::default();
        c.mode = "external".into();
        c.path = r"C:\Users\someone\AppData\Local\Rclone\rclone.exe".into();

        let kept = c.adopt_managed();
        assert_eq!(
            kept.as_deref(),
            Some(r"C:\Users\someone\AppData\Local\Rclone\rclone.exe")
        );
        // 路径已经不再生效，但原样留了一份，想填回去随时能填。
        assert_eq!(c.path, "");
        assert_eq!(
            c.kept_paths,
            vec![r"C:\Users\someone\AppData\Local\Rclone\rclone.exe".to_string()]
        );
        assert_eq!(c.normalized_mode(), "managed");
    }

    /// 同一个路径反复点不该堆出一串重复项（Windows 路径大小写不敏感）。
    #[test]
    fn adopt_managed_dedupes_paths_ignoring_case() {
        let mut c = CoreCfg::default();
        for p in [
            r"C:\Tools\rclone.exe",
            r"c:\tools\RCLONE.exe",
            r"C:\Tools\rclone.exe",
        ] {
            c.path = p.into();
            assert_eq!(c.adopt_managed().as_deref(), Some(p));
        }
        assert_eq!(c.kept_paths.len(), 1, "大小写不同的同一个路径只留一份");
        assert_eq!(c.kept_paths[0], r"C:\Tools\rclone.exe");
    }

    /// 没填路径就没有可搬的东西：返回 `None`，顺带把模式改回托管。
    #[test]
    fn adopt_managed_reports_nothing_to_keep_without_a_path() {
        let mut c = CoreCfg::default();
        c.mode = "external".into();
        c.path = "   ".into();
        assert_eq!(c.adopt_managed(), None);
        assert_eq!(c.normalized_mode(), "managed");
        assert!(c.kept_paths.is_empty(), "空白路径不该进备选");
    }

    /// 备选路径要能存进配置文件并读回来，否则重启后就丢了。
    #[test]
    fn kept_paths_survive_a_config_round_trip() {
        let mut c = CoreCfg::default();
        c.path = r"D:\fork\rclone.exe".into();
        c.adopt_managed();
        let json = serde_json::to_string(&c).unwrap();
        let back: CoreCfg = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kept_paths, vec![r"D:\fork\rclone.exe".to_string()]);
        assert_eq!(back.path, "");
    }

    /// `recorded_flavor` 只在真的存过时给值，空白（老配置）要让调用方去推。
    #[test]
    fn recorded_flavor_is_none_until_explicitly_stored() {
        let mut c = CoreCfg::default();
        assert_eq!(c.recorded_flavor(), None);
        c.flavor = "official".into();
        assert_eq!(c.recorded_flavor(), Some(crate::core::Flavor::Official));
        c.flavor = "  ".into();
        assert_eq!(c.recorded_flavor(), None, "空白等于没存过");
    }

    /// `bind_proxy_slot` 换目标时只清 `applied`，保留脚印（回滚依据）。
    ///
    /// 2026-10-18 修掉的缺陷：清掉 `applied_value` 后，切「不代理」时
    /// `plan_apply` 会判定「从未管理过」，rclone 里的旧代理永远删不掉。
    #[test]
    fn rebinding_keeps_rollback_footprint() {
        let mut cfg = AppConfig::default();
        cfg.proxies.push(ProxyProfile {
            id: "p1".into(),
            name: "n".into(),
            ..Default::default()
        });
        cfg.bind_proxy_slot("r", "p1");
        {
            let b = &mut cfg.proxy_bindings[0];
            b.applied = true;
            b.applied_key = "http_proxy".into();
            b.applied_value = "http://ours:1".into();
            b.original_override = None;
        }
        cfg.bind_proxy_slot("r", "");
        let b = &cfg.proxy_bindings[0];
        assert!(b.is_direct());
        assert!(!b.applied, "换目标后要标成待应用");
        assert_eq!(b.applied_value, "http://ours:1", "回滚依据不能被清掉");
        assert_eq!(b.applied_key, "http_proxy");
        assert!(b.has_footprint());
    }

    #[test]
    fn data_dir_override_needs_a_real_path() {
        // 未设置 / 空白：都不算覆盖，老路径（便携或 %APPDATA%）照旧。
        assert!(override_dir(None).is_none());
        assert!(override_dir(Some("")).is_none());
        assert!(override_dir(Some("   ")).is_none());
        // 绝对路径原样使用。
        let abs = if cfg!(windows) {
            r"D:\qa\rcgui-data"
        } else {
            "/tmp/rcgui-data"
        };
        assert_eq!(override_dir(Some(abs)), Some(PathBuf::from(abs)));
        // 相对路径解析成绝对路径：相对目录会让互斥量、日志目录跟着 cwd 漂。
        let rel = override_dir(Some("qa-data")).expect("相对路径也要认");
        assert!(rel.is_absolute(), "相对覆盖路径应被解析成绝对路径：{rel:?}");
        assert!(rel.ends_with("qa-data"));
    }

    #[test]
    fn scope_separates_different_data_dirs() {
        let a = scope_hash(std::path::Path::new(r"D:\qa\a"));
        let b = scope_hash(std::path::Path::new(r"D:\qa\b"));
        assert_eq!(a.len(), 16);
        assert_ne!(a, b, "不同数据目录必须算出不同的隔离键");
        // 大小写与结尾分隔符不该改变归属（同一个目录必须同键，否则单实例失效）。
        let a2 = scope_hash(std::path::Path::new(r"d:\QA\a\"));
        assert_eq!(a, a2, "同一目录的不同写法要聚成同一个键");
    }
}
