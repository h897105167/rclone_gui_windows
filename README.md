# RCGUI

Windows 上的 rclone 图形管理工具（RCGUI）。Rust + [Slint](https://slint.dev) 构建，
插件系统基于 WASM。插件与本体彻底分离：**每个插件一个独立仓库**，本体只保留一份
锁文件 [plugins/manifest.json](plugins/manifest.json)，构建时按锁文件从各插件仓库的
Release 下载并校验。

> **RCGUI 不拥有 rclone 的进程生命周期**：它不启动、不停止、不重启 rclone。你需要自己安装并运行 rclone，RCGUI 只连接你指定的 RC endpoint。

> **注意**：本仓库不是旧的 Node.js + Vue 3 网页版（`rcdeck`）。旧版已放弃，仅可作为设计参考。

## 功能概览

| 模块 | 说明 |
|---|---|
| 仪表盘 | 运行版本、传输速度、挂载/传输计数 |
| 文件 | 远程目录浏览、新建/重命名/删除/复制/复制直链 |
| 远程 | 基于 rclone `config/providers` 的动态参数表单（非硬编码类型列表） |
| 挂载 | 把远程挂载为本地盘符（需要 WinFsp），支持自动挂载预设 |
| 服务 | WebDAV / SFTP / HTTP / FTP 等 serve 端点，支持自动启动预设 |
| 传输 | 活动传输与后台任务、全局限速 |
| 插件 | 运行时加载的 WASM 插件包；界面由插件 `config.json` 的声明式控件渲染，本体不认识任何插件 id |
| 设置 | 主题（跟随系统/浅色/深色）、强调色、开机自启、静默启动、内嵌 HTTP |
| 语言 | 中文 / English 两套界面。跟随系统或手动切换，**切换后立即生效、无需重启**；内置插件面板的文案也随之切换（插件包本身不改，词典在宿主侧） |

内置插件（各自独立仓库，发布在自己的 Release 上）：

| 插件 | 仓库 | 说明 |
|---|---|---|
| 图床 | <https://github.com/h897105167/rcgui-plugin-imagehost> | 上传、SHA-1 去重、索引、直链、短链 |
| 一键 rclone | <https://github.com/h897105167/rcgui-plugin-oneclick> | 环境自检、下载安装 rclone、创建远程、常用操作 |

## 环境要求

- **Rust 工具链**（MSVC）：<https://rustup.rs>
- **WinFsp**：仅在「挂载为本地盘符」时需要 <https://winfsp.dev>
- **wasm32-wasip1 目标**：只在**开发插件**时需要（`rustup target add wasm32-wasip1`）；
  构建本体不需要，插件是现成的 Release 资产。

## 快速开始

```powershell
git clone https://github.com/h897105167/rclone_gui_windows.git
cd rclone_gui_windows

# 构建（会先按 plugins/manifest.json 拉取插件资产，再构建应用）
.\scripts\build.ps1

# 运行
.\target\debug\rclone-gui.exe
```

或者一步构建并运行：

```powershell
.\scripts\build.ps1 -Run
```

Release 构建：

```powershell
.\scripts\build.ps1 -Release
.\target\release\rclone-gui.exe
```

常用参数：

| 参数 | 作用 |
|---|---|
| `-Release` | release 构建 |
| `-Run` | 构建后直接启动 |
| `-SkipPlugins` | 不检查/不下载插件资产（只改本体时用；资产缺失会有警告） |
| `-ForcePlugins` | 无视本地已就绪状态，强制重新下载 |

> 若提示脚本被执行策略阻止，可在当前会话临时放开：
> `Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass`

## 手动构建（不用脚本）

本体**不含插件源码**，所以第一步是拿插件资产，顺序不能反：

```powershell
# 1) 按锁文件下载并校验插件资产 → crates/app/assets/plugins/<id>/
.\scripts\fetch_plugins.ps1

# 2) 构建应用（build.rs 会把 assets/plugins 复制到 exe 同级目录）
cargo build -p rclone-gui

# 运行
.\target\debug\rclone-gui.exe
```

某个插件仓库发了新版本时，改 `plugins/manifest.json` 里的 `tag` 与 `sha256`，
再 `.\scripts\fetch_plugins.ps1 -Force` 即可。插件的完整打包/发布流程见
[docs/PLUGIN.md](docs/PLUGIN.md)。

## 插件机制（一句话版）

插件是运行时加载的 WASI 包（目录形态：`config.json` + 入口 wasm + `check.sha256`）。
宿主与插件之间只有一条 JSON 通道：插件通过 `need` 申请能力（`kv` / `fs` / `http` /
`proc` / `rclone` / `ui` …），宿主校验 `capabilities` 白名单后执行；插件页的界面
完全由 `config.json` 的 `plugin_setting` 控件描述驱动。**新增插件不需要改本体源码。**

框架、命名、接入规则、能力清单、发布流程都在
**[docs/PLUGIN.md](docs/PLUGIN.md)**。

## 关于 rclone（外部进程）

RCGUI **不自带、不解压、不启动** rclone。使用前请自行安装 rclone 并让它提供 RC 接口，例如：

```powershell
rclone rcd --rc-addr 127.0.0.1:5574 --rc-user gui --rc-pass <你的密码>
# 或者在你的挂载命令上加 --rc：
rclone mount teldrive: T: --rc --rc-addr 127.0.0.1:5574 --rc-user gui --rc-pass <密码>
```

然后在 RCGUI「设置 → rclone 连接」里填该地址/端口/用户名/密码，点「连接 / 握手」。
连不上时 RCGUI 进入离线模式（可继续编辑/导入导出本地档案），**不会**回退成替你启动 rclone。

关闭 RCGUI 只停止轮询、释放连接：**不会**调用 `core/quit`、不会杀进程、不会卸载挂载或停止 serve。

## 内嵌 HTTP 服务

默认监听 `127.0.0.1:5273`（可在设置中修改）。路由是通用的，本体不认识任何插件的业务路径：

| 端点 | 说明 |
|---|---|
| `GET /api/health` | 健康检查 |
| `ANY /plugin/{id}` | 交给 `id` 对应的插件（需其 `config.json` 声明 `"http_entry": true`） |
| `ANY /plugin/{id}/{*rest}` | 同上，路径/查询串/请求头/原始 body 原样转交 |

路径表、鉴权、响应格式都由插件自己决定。图床插件的上传与直链端点就在它自己的
仓库里（`POST /plugin/imagehost/upload`、`GET /plugin/imagehost/i/{root}/{path}` …）。

`rclone rcd` 默认监听 `127.0.0.1:5572`；本机若用 `--rc-addr 127.0.0.1:5574` 启动，就在设置里填这个地址。

## 数据目录

配置与插件数据位于 `%APPDATA%\\RCGUI`（首次启动时从旧的 `%APPDATA%\\rclone-gui` **复制**迁移，旧目录保留可回滚）：

- `config.json` — 应用配置（连接地址、主题、内嵌 HTTP 等）
- `profiles\default.json` — 档案：远程元信息、**记住的挂载/服务**（连接成功后自动恢复）
- `plugins\<id>\` — 插件包与各插件的 `kv.json`（内置插件首次运行时从 exe 同级
  `assets\plugins\` 释放到这里，之后与用户自己装的插件完全同构）
- `cache\` — 插件申请的缓存（例如图床直链缓存）

**便携模式**：在 `rclone-gui.exe` 同目录放一个名为 `portable` 的空文件，
数据目录会改为 `exe目录\data`。

## 开发

```powershell
cargo build --workspace          # 构建全部 crate
cargo test --workspace           # 运行单元测试
cargo run -p plugin_host --example protocol_test    # 插件协议测试
```

### 项目结构

```text
crates/
  app/       # Slint UI、主程序、配置、托盘、启动项、内嵌 HTTP
  rc/        # RC 客户端、连接状态、握手与能力探测
  profile/   # 版本化档案、rcgui.conf 导入导出、恢复脚本生成
  rclone/    # legacy：rclone 进程管理残留（不再启动进程）
  plugin/    # 插件宿主（wasmtime + WASI）与能力 API、config.json 校验
plugins/
  manifest.json  # 插件锁文件（id / repo / tag / sha256），本仓库唯一的插件相关文件
scripts/     # build.ps1 / fetch_plugins.ps1 等构建脚本
docs/
  PLUGIN.md  # 插件框架与接入规则（写插件先看这个）
  CHANGELOG.md  # 按日期记录的阶段性变更
```

插件源码不在本仓库，见上面的内置插件表。

## 变更记录

按日期记录每个阶段做了什么，不维护语义化版本号：[docs/CHANGELOG.md](docs/CHANGELOG.md)。

## 安全提示

- RC 密码保存在 `%APPDATA%\RCGUI\config.json`（明文，属用户目录，不入库）。
- 插件在 WASI 沙箱内运行（无文件系统/网络预开放），只能发起 `config.json.capabilities`
  里声明过的能力组，未声明的一律被宿主拒绝；另有 fuel、栈、轮数与超时上限。
- 插件包有双重校验：包内 `check.sha256` 校验入口文件，`plugins/manifest.json` 的
  `sha256` 校验下载到的 zip。
- `fs.*` / `proc.*` / `http.*` 是高危能力（前者能读写文件、后者能执行外部程序）。
  装插件前先看它声明了什么，以及它的仓库是否可信。
- 导出 `rcgui.conf` 默认剥离凭据；勾选「包含凭据」会二次确认，且 rclone 的
  `obscure` 只是可逆混淆、并非加密。
