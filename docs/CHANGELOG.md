# 变更记录

本文件按 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 组织，版本号遵循
[SemVer](https://semver.org/lang/zh-CN/)。版本从哪来、什么算"已发布"、
`developing` / `planned` 这些状态词怎么用，见 [VERSIONING.md](VERSIONING.md)。

要点先说清楚：

- 当前树里的版本是 **0.3.0，开发中**，不是已发布版本。
- 唯一带 GitHub Release 产物的版本是 **v0.2.1**（资产名是旧的
  `rclone-gui-v0.2.1-windows-amd64.zip`）。新的
  `RCGUI-<version>-windows-x64-*` 命名从 0.3.0 开始用，见 [RELEASE.md](RELEASE.md)。
- 本仓库早期按「会话」推进、没有版本号。为了让历史可查，那段按日期的记录**原样保留在文末**，
  不强行分摊到各个版本下——`v0.1.0` / `v0.2.0` / `v0.2.1` 三个 tag 打在 i18n 分支合并前后，
  版本号 bump 的提交出现在 `v0.2.1` 之后，硬套版本只会编出假的归属。

## [Unreleased]

0.3.0 的功能与文档改动，状态 `developing`：已合进 `main` 的写在下面，
只有设计、还没实现的写在 [ARCHITECTURE.md](ARCHITECTURE.md) 的 `规划` 行，不进这里。

### Added

- 受管内核（managed core）：应用默认自己启动并管理一个 `rclone.exe`，同时仍支持连接
  用户已有的外部 RC 端点；结束进程只允许针对自己拉起的那个实例。
  （设计见 [ARCHITECTURE.md §1.1](ARCHITECTURE.md)，实现由核心会话完成，尚未验收。）
- 插件面板支持图片行、多行文本字段与进度列。
- 插件商店：`docs/store/index.json` 索引 + 静态商店页（GitHub Pages），
  收录 `imagehost 0.6.1` 与 `teldrive 0.3.0`，二者 sha256 均已对真实 Release 资产核验。
- CLI 规格：[CLI.md](CLI.md) 定义 `--version`、`--diagnostics --json`、
  `--validate-config <path>`、`--list-plugins`、`--export-accessibility-tree <path>`。
  文档同时说明静态可访问性树与真实 UIA 树的区别。
- 发布产物分两种，命名固定为 `RCGUI-<version>-windows-x64-setup.exe` 与
  `RCGUI-<version>-windows-x64-portable.zip`，附 `SHA256SUMS`。portable 包里带受管内核、
  内核许可证和选定插件（不含 onerclone）；安装包按用户目录安装，不覆盖 `%APPDATA%` 下的数据。
  详见 [RELEASE.md](RELEASE.md)。
- 可复现打包：`scripts/fetch_core.ps1` 按 [../packaging/core-manifest.json](../packaging/core-manifest.json)
  里的固定版本与 sha256 取 rclone（当前 v1.76.0），`scripts/build-release.ps1` 逐文件复核
  插件哈希、缺内核或许可证直接失败。
- 插件商店投稿对任何 GitHub 账号开放：地址必须指向声明仓库自己的 Release 资产，
  人工审核看代码、许可证与申请的权限。字段与审核门槛见
  [store/README.md](store/README.md)，机器校验是 `scripts/validate-store-index.ps1`。

### Changed

- `plugins/manifest.json` 锁定 teldrive `v0.3.0`。
- `AGENTS.md` 重写：旧的「应用不得启动或停止 rclone」约束替换为受管内核约定
  （默认可管理自己的内核、外部端点只断开、不得接管他人进程）。
- 新增 [ARCHITECTURE.md](ARCHITECTURE.md)（分层与并发/缓存/凭据/可访问性/测试/发布契约）、
  [CONTRIBUTING.md](../CONTRIBUTING.md)、[VERSIONING.md](VERSIONING.md)、
  [PERFORMANCE.md](PERFORMANCE.md)、[RELEASE.md](RELEASE.md)。
- CI 从只在 `session-*` 分支跑，改为 `push main/master`、`pull_request`、`workflow_dispatch`，
  增加格式检查、依赖缓存、最小权限与显式超时；新增 tag `v*` 触发的发布工作流。

### Fixed

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
