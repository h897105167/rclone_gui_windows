# RCGUI

Windows 上的 rclone 图形管理工具（RCGUI）。Rust + [Slint](https://slint.dev) 构建，插件系统基于 WASM（首个插件为图床）。\n\n> **RCGUI 不拥有 rclone 的进程生命周期**：它不启动、不停止、不重启 rclone。你需要自己安装并运行 rclone，RCGUI 只连接你指定的 RC endpoint。

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
| 插件 | WASM 插件（内置图床：上传、SHA-1 去重、索引、直链代理、Token） |
| 设置 | 主题（跟随系统/浅色/深色）、强调色、开机自启、静默启动、内嵌 HTTP |

## 环境要求

- **Rust 工具链**（MSVC）：<https://rustup.rs>
- **wasm32-wasip1 目标**（插件需要）：`rustup target add wasm32-wasip1`
- **WinFsp**：仅在「挂载为本地盘符」时需要 <https://winfsp.dev>

## 快速开始

```powershell
git clone https://github.com/h897105167/rclone_gui_windows.git
cd rclone_gui_windows

# 构建（会先构建 WASM 插件，再构建应用）
.\scripts\build.ps1

# 3) 运行
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

> 若提示脚本被执行策略阻止，可在当前会话临时放开：
> `Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass`

## 手动构建（不用脚本）

**顺序很重要**：必须先构建 WASM 插件，再构建应用。因为
`crates/app/src/main.rs` 用 `include_bytes!(env!("IMAGEHOST_WASM"))` 把插件编译进程序，
而 `crates/app/build.rs` 是去 `target/wasm32-wasip1/<profile>/imagehost.wasm` 找该文件的。
顺序反了会编译失败或内置插件缺失。

```powershell
# 1) 先构建 WASM 插件
cargo build -p imagehost-plugin --target wasm32-wasip1

# 2) 再构建应用
cargo build -p rclone-gui

# 运行
.\target\debug\rclone-gui.exe
```

常用参数：

```powershell
.\target\debug\rclone-gui.exe --silent   # 静默启动（仅托盘，不显示窗口）
```

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

用于图床上传与直链，默认监听 `127.0.0.1:5273`（可在设置中修改）：

| 端点 | 说明 |
|---|---|
| `GET /api/health` | 健康检查 |
| `POST /api/plugin/imagehost/upload` | 上传（multipart 或 raw body + query） |
| `POST /upload` | 旧版 Obsidian 插件的兼容别名（raw body + query） |
| `GET /i/{rootId}/{path}` | 图床直链代理（磁盘缓存 + rclone 回源） |

`rclone rcd` 默认监听 `127.0.0.1:5572`；本机若用 `--rc-addr 127.0.0.1:5574` 启动，就在设置里填这个地址。

## 数据目录

配置与插件数据位于 `%APPDATA%\\RCGUI`（首次启动时从旧的 `%APPDATA%\\rclone-gui` **复制**迁移，旧目录保留可回滚）：

- `config.json` — 应用配置（连接地址、主题、内嵌 HTTP 等）
- `profiles\default.json` — 档案：远程元信息、**记住的挂载/服务**（连接成功后自动恢复）
- `plugins\` — WASM 插件与各插件 KV
- `cache\` — 图床直链缓存

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
  plugin/    # WASM 插件宿主与能力 API
plugins/
  imagehost/ # 图床 WASM 插件
scripts/     # 构建脚本（先 WASM 后应用）
```

## 安全提示

- RC 密码保存在 `%APPDATA%\RCGUI\config.json`（明文，属用户目录，不入库）。
- 插件在 WASI 沙箱内运行（无文件系统/网络预开放），并有 fuel 执行上限。
- 直链路径已做规范化，防止 `..` 目录穿越。
- 删除图床文件：未配置回收站目录时为**永久删除**，界面会明确提示。
- 导出的 `rcgui.conf` 默认剥离凭据；勾选「包含凭据」会二次确认，且 rclone 的 `obscure` 只是可逆混淆、并非加密。
