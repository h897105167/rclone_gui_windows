# 变更记录

本文件按 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 组织，版本号遵循
[SemVer](https://semver.org/lang/zh-CN/)。版本从哪来、什么算"已发布"、
`developing` / `planned` 这些状态词怎么用，见 [VERSIONING.md](VERSIONING.md)。

要点先说清楚：

- 当前树里的版本是 **0.3.4，开发中**，不是已发布版本；对外发布过的最新版本是 **v0.3.3**。
  `v0.3.0` 和 `v0.3.1` 都没有过 tag：下文习惯写作 0.3.0 的那批内容，就是随 0.3.2 发布的那一批。
- 带 GitHub Release 产物的版本有 **v0.2.1**、**v0.3.1**、**v0.3.2** 和最新的 **v0.3.3**。旧的
  `rclone-gui-v0.2.1-windows-amd64.zip` 命名只用在 v0.2.1 上；新的
  `RCGUI-<version>-windows-x64-*` 命名从 0.3.2 起，见 [RELEASE.md](RELEASE.md)。
- 本仓库早期按「会话」推进、没有版本号。为了让历史可查，那段按日期的记录**原样保留在文末**，
  不强行分摊到各个版本下——`v0.1.0` / `v0.2.0` / `v0.2.1` 三个 tag 打在 i18n 分支合并前后，
  版本号 bump 的提交出现在 `v0.2.1` 之后，硬套版本只会编出假的归属。

## [Unreleased]

这一批的功能与文档改动（文档里习惯叫 0.3.0；其中绝大部分随 0.3.2 发布，0.3.3 和 0.3.4 收的是
发布之后验收发现的问题），状态 `developing`：
已合进 `main` 的写在下面，只有设计、还没实现的写在 [ARCHITECTURE.md](ARCHITECTURE.md) 的 `规划` 行，不进这里。

### Added

- 受管内核（managed core）：应用默认自己启动并管理一个 `rclone.exe`，同时仍支持连接
  用户已有的外部 RC 端点；结束进程只允许针对自己拉起的那个实例。
  （设计见 [ARCHITECTURE.md §1.1](ARCHITECTURE.md)，实现由核心会话完成，尚未验收。）
- 插件面板支持图片行、多行文本字段与进度列。
- 插件商店：`docs/store/index.json` 索引 + 静态商店页（GitHub Pages），
  收录 `imagehost 0.6.1`、`kvault 0.3.1` 与 `teldrive 0.3.0`，三者 sha256 均已对真实 Release
  资产核验；`kvault` 的 `min_gui_version` 写 `0.3.2`，因为它的面板要在用户打字时刷新，
  更低的构建装上去只会看到按钮一直是灰的。
- CLI 规格：[CLI.md](CLI.md) 定义 `--version`、`--diagnostics --json`、
  `--validate-config <path>`、`--list-plugins`、`--export-accessibility-tree <path>`。
  文档同时说明静态可访问性树与真实 UIA 树的区别。
- 发布产物分两种，命名固定为 `RCGUI-<version>-windows-x64-setup.exe` 与
  `RCGUI-<version>-windows-x64-portable.zip`，附 `SHA256SUMS`。portable 包里带受管内核、
  内核许可证和选定插件（不含 onerclone）；安装包按用户目录安装，不覆盖 `%APPDATA%` 下的数据。
  详见 [RELEASE.md](RELEASE.md)。
- 可复现打包：`scripts/fetch_core.ps1` 按 [../packaging/core-manifest.json](../packaging/core-manifest.json)
  里的固定版本与 sha256 取 rclone（当前默认 v1.76.0-extra），`scripts/build-release.ps1` 逐文件复核
  插件哈希、缺内核或许可证直接失败。
- 内核双来源：清单升到 `schema_version: 2`，`sources` 下同时钉住**增强版**
  `gulp79/rclone-extra v1.76.0-extra`（rclone v1.76.0 的分支，多出 alist、alldebrid、
  teldrive、terabox 等后端）和官方原版 rclone v1.76.0，`default_source` 是增强版。
  `-CoreSource` 选来源，写错直接失败而不退回默认；非默认来源的产物带 `-<来源>` 后缀，
  两个来源可以并排放在同一个 `dist\` 里而不互相覆盖。`fetch_core.ps1 -CheckManifest`
  离线校验清单结构，CI 每次编译测试都跑。增强版不发布 SHA256SUMS，所以它的校验锚点是
  清单里钉住的 zip sha256，`.fetched.json` 会记下 `checksums_check: no-official-sums-published`。
- 插件商店投稿对任何 GitHub 账号开放：地址必须指向声明仓库自己的 Release 资产，
  人工审核看代码、许可证与申请的权限。字段与审核门槛见
  [store/README.md](store/README.md)，机器校验是 `scripts/validate-store-index.ps1`。
- 插件可以自己声明远程类型：`remote_types[]` 增加 `marker`（识别这类远程的标记参数名）
  与 `ops`（`remote.list` / `remote.upload` / `remote.delete` / `remote.rclone-write`），
  再用 `adopt_legacy` 声明要接管的旧设置命名空间。主程序只按这些声明路由，不认插件专名；
  某个远程属于哪个插件记在 `remote_owners.json`，停用或卸载插件后这个远程不会被当成普通
  WebDAV 打开，普通 WebDAV 远程不受影响。
- K-Vault 的站点接口、上传、删除与设置迁移搬进 K-Vault 插件，主程序不再含这份业务代码。
  插件没装或停用时，相应远程在远程列表、文件页、挂载与启动恢复里都会说明原因，而不是静默失败。
- 「文件夹同步」从「传输」页独立成左侧「运行时」组里的一页：任务列表、新建与运行/暂停/删除
  都搬了过来，传输页只剩传输任务本身。
- 文件夹同步可以在本机文件夹有变化时提前跑一轮（「自动：开/关」）。变化由系统通知送来
  （Windows 的 `ReadDirectoryChangesW`，经 `notify`），不再比对文件数与字节数——那种比对看不出
  同大小的改写、改名，也看不出改了又改回去。改了立刻跑会把传输打成一串碎请求，所以等改动停
  3 秒后再跑，两次之间也不会并行。通知本身靠不住：程序刚接手、缓冲区溢出、目录被删掉又重建的
  时候，历史上到底改了什么无从知道——这些情况一律记成「欠一轮」，启动后先跑一轮确认，关机期间
  的变化也就跟着补上；不再用「最新修改时间是否晚于上次同步」去猜，因为改名、保留时间戳的复制、
  关机时的删除它都看不出来。通知队列有上限，满了先记一笔再补跑，文件刷屏也不会把内存吃光。
  这一轮没真的跑起来时，「欠着一轮」会还给任务，并按卡在哪一步分开处理：内核正在跑同一个目录
  就等它真的结束、之后只补一次，期间不自己重起；内核还没连上、rclone 拒绝就退避重试，间隔逐次
  拉长到 15 分钟封顶；镜像任务的本机目录空着或读不出来就只提醒，等下一次变化、目录回来，或者
  用户自己按「运行」，没人改就永远不动。只对「本地 → 云端」的任务生效：
  反方向时本机文件的变化正是用户自己的编辑，拿它触发同步会把编辑覆盖掉。镜像任务（会在目标端
  删文件的那种）默认不开自动同步，要用得逐条手动打开——一次误读的代价太大；本机目录变空或者
  读不出来时，镜像任务只提醒、不自动跑（读不出来的后果和空目录一样：照着删就是清空云端）。没
  选过的老配置按「只复制任务开、镜像任务不开」处理。目录不存在、没有读权限这类情况只说一次，
  恢复后自动继续。

### Fixed

- 插件面板里填完站点地址和令牌后，「测试连接」「新建远程」仍是灰的，状态栏还停在
  「先填站点地址；建远程还需要令牌」。原因两头都在：主程序每次按键只把文本存进表单，
  从不重新向插件要状态；插件回答 `state` 时又只读已保存的站点，读不到正在输入的东西。
  现在按键后就着现有行就地刷新状态与按钮，正在编辑的那一行保留用户输入的原文，
  插件那边则让表单优先于已保存的值。K-Vault 插件 0.3.1 起要求本程序 0.3.2 及以上。
- 站点设置迁移（插件接管旧命名空间时）会把命名空间里的每一格都当成一条远程记录，
  其中包括主程序存的 `form`——用户刚输入的令牌就在里面——和插件的 `_state`。
  导出的清单里因此会凭空多出一条叫 `form` 的记录带着令牌；现在这两格都跳过。
- 图床插件往插件托管的远程（如 K-Vault）上传一律失败：`rclone.upload_file` 原来不看目标
  远程归谁管，直接把字节交给内核 `copyfile`，而这类远程在内核眼里只是普通 WebDAV 地址，
  站点只认它自己的上传接口，于是回 `403 Forbidden`（`this WebDAV endpoint is read-only`）。
  现在这个能力先问清远程归谁：插件托管的转给该插件声明的 `remote.upload`，普通远程照旧
  `copyfile`；插件没声明要接管、或认领它的插件不在时直接报错，不退回原生访问。插件回包里
  的 `name`/`link`/`deduped` 原样交给调用方，图床因此能拿到站点直链，而不是本机代理地址。
- 上一版把这条路径修通了，却把授权键写死成 `remote-0`：授权表按 `(插件, 键)` 记，两个上传
  同时在跑时，后登记的会把前一条顶掉，于是发出去的可能不是这一张图，撤销的也不是自己那条。
  现在每个请求领一个自己的 ASCII 键（`cap-<pid>-<序号>`），结束、失败、取消都只收自己的
  授权；本机临时副本也改成带序号的名字，并且离开作用域就删，取消或出错不再留下文件。
- 单字母远程名一律当盘符，会让合法的 `x:` 绕过归属检查、直接按原生方式写。现在只有
  「盘符 + 分隔符」（`C:\`、`D:/`）才认作本机完全路径，`x:` 这种有歧义的照样去问归属：
  没人认领就按原生走，有插件认领就必须经插件发，不会静默退回原生写。
- K-Vault 远程的「文件」页把根目录显示成 `k-vault`，实际列出的却是 `k-vault/image_bed`：
  插件里同一个「空路径」兜底既用在浏览上也用在上传上，于是浏览被拽去图床目录。现在两者
  分开——浏览与删除拿到的空路径就是远程真正的根，上传仍落到设置里的上传目录（图床目录），
  所以图床的默认上传位置、已有文件和删除目标都不变。

### Changed

- 版本号升到 **0.3.4**（补丁位）：0.3.3 已经发布，这一轮同样收的是发布之后验收提出的问题。
- 版本号升到 **0.3.3**（补丁位）：0.3.2 已经发布，这一轮收的是发布之后验收提出的问题，
  所以再抬一格，让还在 0.3.2 上的机器能看到这次更新。
- 版本号升到 **0.3.2**（补丁位）。`0.3.0` 与 `0.3.1` 都没有打过 tag，这一批内容随 `0.3.2`
  发布；插件按自己真正需要的门槛声明 `min_gui_version`：要新插件协议的写 `0.3.1`，
  还要宿主在用户打字时刷新面板状态的写 `0.3.2`，版本更低的构建在商店安装与本地包加载
  两处都会明确拒绝，并提示先升级本程序。
- `plugins/manifest.json` 锁定 teldrive `v0.3.0`。
- `AGENTS.md` 重写：旧的「应用不得启动或停止 rclone」约束替换为受管内核约定
  （默认可管理自己的内核、外部端点只断开、不得接管他人进程）。
- 新增 [ARCHITECTURE.md](ARCHITECTURE.md)（分层与并发/缓存/凭据/可访问性/测试/发布契约）、
  [CONTRIBUTING.md](../CONTRIBUTING.md)、[VERSIONING.md](VERSIONING.md)、
  [PERFORMANCE.md](PERFORMANCE.md)、[RELEASE.md](RELEASE.md)。
- CI 从只在 `session-*` 分支跑，改为 `push main/master`、`pull_request`、`workflow_dispatch`，
  增加格式检查、依赖缓存、最小权限与显式超时；新增 tag `v*` 触发的发布工作流。

### Fixed

- 打包步骤成功却被 CI 判成失败：`build-release.ps1` 原来用 `$LASTEXITCODE -ne 0` 判断每个子步骤，
  而 `fetch_plugins.ps1` 成功时只打印结果、从不设置这个变量，调用方读到的是 `$null`（或上一条原生
  命令留下的旧值），`$null -ne 0` 为真，于是插件明明已就绪、打包仍报失败（run 45793075820 /
  job 139823454789）。现在四个调用点（取插件、`cargo build`、取内核、ISCC）统一走 `Invoke-Native`：
  调用前把 `$LASTEXITCODE` 清零，调用包进 try/catch 接住子脚本的 `throw`（`throw` 时这个变量仍是 0，
  只靠退出码判断会漏掉）；`build-release.ps1`、`build.ps1`、`fetch_core.ps1`、`fetch_plugins.ps1`
  成功时都显式 `exit 0`，不再让调用方猜。
- 文档中"插件会被嵌进二进制、必须在 `cargo build` 之前就位"的过时说明已改正：
  插件由 `scripts/fetch_plugins.ps1` 拉取并校验，`build.rs` 只负责把资产复制到二进制旁边。

## [0.2.1] - 2026-10-26

唯一发布过 Release 产物的版本（`rclone-gui-v0.2.1-windows-amd64.zip`）。

### Added

- 插件自带译文：宿主每次调用插件都带 `params.lang`，插件自行返回中文或英文；
  宿主侧的插件词典（`plugin_i18n`）整块删除。`config.json` 的展示字段与 `tags`
  可写成 `{"zh":…,"en":…}`，选项可写 `{"value":…,"label":{…}}`，落盘值仍是原文。
- 图床插件 v0.6.1、一键 rclone 插件 v0.3.3 发布，`plugins/manifest.json` 锁到新版。

### Fixed

- `crates/plugin/examples/protocol_test.rs` 上一轮 i18n 包装把测试数据当文案翻译，
  导致 `format argument must be a string literal` 编译失败。

## 更早的版本

`v0.1.0`（2026-10-25）与 `v0.2.0`（2026-10-26）两个 tag 存在，但没有任何 GitHub Release
产物，且 tag 与版本号 bump 的先后顺序无法从提交历史可靠还原（见文首说明）。
它们对应的功能包含在下面的按日期记录里。

## 历史记录（按日期，早于版本约定）

原始文本保留，仅调整了标题层级。

### 2026-10-26

- **插件自带译文**：宿主每次调用插件都带 `params.lang`，插件按它回答中文或英文；宿主侧
  的插件词典（`plugin_i18n`）整块删除，翻译不再由宿主代劳。`config.json` 的展示字段、
  `tags` 可写 `{"zh":…,"en":…}`，选项可写 `{"value":…,"label":{…}}`，落盘值仍是原文。
- 图床插件 **v0.6.1**、一键 rclone 插件 **v0.3.3** 发布，`plugins/manifest.json` 已锁新版。
- 修 `crates/plugin/examples/protocol_test.rs`：上轮 i18n 包装误把测试数据当文案翻，
  导致 `format argument must be a string literal` 编不过。
- `crates/plugin/src/config.rs` 的 `tags` 支持语言映射（`de_localized_list`）。
- 文档：`docs/PLUGIN.md` 补「展示字段的多语言」与 `params.lang`。

### 2026-10-24

- **插件与本体彻底分离**：插件源码移出本仓库，构建前由 `scripts/fetch_plugins.ps1`
  按 `plugins/manifest.json` 从各插件自己的 Release 拉 zip 并校验 sha256，本仓库只留锁文件。
- 移除最后一处插件 id 特判与废弃属性；补齐插件 `http_entry`（内嵌 HTTP 入口）文档。
- 图床面板收敛为「远程 + 路径 + 上传 + 短链」，补浏览、路径历史、去重与 loading 态。
- 插件 `config.json` 的 `body.choices` 对 `select` 生效；图床插件锁到 v0.4.0。
- 修插件面板串台、远程下拉失效；`build.ps1` 不再把「准备插件资产」误判为失败。
- 文档：补 `http.request` / `ui.copy` / `rclone.cat` / `fs.read_b64` 能力说明。

### 2026-10-23

- 图床工具：上传图片取直链，按模板换短链；修 AList 直链取值，支持 teldrive 分享链接。
- 新增远程类型 `k-vault`：只读 DAV 浏览 + 站点 API 上传（索引列目录、幂等上传、真删除）。

### 2026-10-22

- 文件页双栏：排序子菜单、本机多选、上传到/下载到指定目录、双栏拖放。
- 传输记账：移动不再被标成重命名；后台任务标签新增重命名 / 移动 / 删除。

### 2026-10-21

- 文件页上传 / 多选 / 排序 / 空白右键菜单；传输页显示文件名并可取消；新增本机双栏。
- 远程编辑器按 `IsPassword` 遮码，已存密钥不再回显。
- 插件拉起 rclone 后按间隔重连，补启动竞态；修一键 rclone 鉴权 403 的根因。
- `Duration` / `SizeSuffix` 带单位落盘，修掉 WebDAV 第二次列目录卡死。
- CI：测试与构建步骤的退出码透出，失败时真的把流水线标红。

### 2026-10-19

- 界面改为 rclone 状态投影，修掉「挂了盘但挂载数显示 0」；回到前台立即对齐一次。
- 轮询刷新走既有筛选，搜索框不再被 1s 轮询刷回全量。
- RC 地址只在已连接时显示；修内嵌 HTTP 覆盖端点。
- 插件卸载 / 恢复后列表立即刷新。

### 2026-10-18

- **插件体系 v2**：三件套可卸载插件 + 声明式面板，卸载写墓碑、可恢复（规则见 `docs/PLUGIN.md`）。
- **按 remote 分配代理** + 统一热重载；FTP / SFTP 按类型改写原生代理键，附带实测报告
  （`docs/proxy-verification.md`，其中 SOCKS5 的键名与预期不同）。
- 文件路径面包屑改为左对齐，并按可用宽度折叠祖先。

### 2026-10-17

- **改造为 RCGUI**：RC 端点为外部 rclone，界面只做状态投影（0.3.0 起改为默认受管内核，
  外部端点仍可用）。
- 修挂载成功后不提示 / spinner 不消失、挂载后立即卸载却误报「盘符未出现」。

### 2026-10-16

- Slint UI 五视图（Remotes / Mounts / Serves / Transfers / Settings）+ 完整桌面应用装配。
- 传输页重写（远程限速 / 服务限速 / 后台任务）；tick 任务防重入 + 超时，修掉 rclone 挂起时
  任务堆积导致的内存暴涨。
- 免终端启动、退出清理、自动挂载 / 服务、远程路径浏览与级联下拉、Spinner、挂载点规范化。
- UI 全面优化：MDL2 图标、手风琴式插件面板、主题色模块、紧凑布局。

### 2026-10-15

- 初始骨架：Rust + Slint 的 rclone GUI。

[Unreleased]: https://github.com/h897105167/rclone_gui_windows/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/h897105167/rclone_gui_windows/releases/tag/v0.2.1
