# RCGUI 重构 · 阶段 1：现状盘点与拆分计划

> 依据：`AGENTS.md`（RCGUI 重构指南）§7.1「Inventory：产出依赖地图与拆分计划，不删文件」。
> 本文档**只做盘点**，未删除、未重命名、未改动任何现有源码。
> 盘点时间：2026-10-17 · 仓库 `rclone_gui_windows` · HEAD `a463f90` · 工作树含大量未提交改动。

---

## 0. 盘点方法与边界

- 只读检查：`git status` / `git diff --stat`、源码通读、`grep` 交叉引用、目录与产物尺寸统计。
- 对运行中的 RC 端点做了**只读**探测（`core/version`、`config/paths`、`rc/list`），用于确认现有 rclone 的能力面，未执行任何修改型调用。
- **未**运行 `cargo build` / `cargo test`（AGENTS §8：除非用户明确要求）。**未**删除 `vendor/`、`.git`、用户数据目录。
- 探测过程中从 `%APPDATA%\rclone-gui\config.json` 读取了 RC 凭据用于鉴权，凭据未打印、未写入任何文件或日志。

---

## 1. 工作树与运行环境现状（改动前必须知道的事实）

### 1.1 Git 状态

| 项 | 值 |
|---|---|
| HEAD | `a463f90 fix(app): 彻底移除 job_list/job_status 轮询…` |
| 已修改未提交 | 12 个文件，`+2512 / -485` |
| 未跟踪 | 11 项：`AGENTS.md`、`README.md`、`进度总结.md`、`.gitattributes`、`scripts/`、`vendor/`、`crates/plugin/src/package.rs`、`crates/plugin/src/zip.rs`、`crates/plugin/examples/{pkg_probe,pkg_scan,zip_probe}.rs` |

改动集中在 `crates/app/src/main.rs`（+1785）、`crates/app/ui/main.slint`（+735）、`crates/rclone/src/proc.rs`（+274）。**这些是用户尚未提交的工作成果，任何重构都必须在它们之上增量修改，不得回滚。**

### 1.2 运行时状态（影响后续阶段的构建与验证）

| 对象 | 状态 | 影响 |
|---|---|---|
| `rclone-gui` PID 22616 | 正在运行 | **锁定 `target\debug\rclone-gui.exe`**，直接 `cargo build` 会报 `os error 5 / 拒绝访问`。阶段 2 起建议用 `CARGO_TARGET_DIR=target-rcgui` 旁路构建，避免要求用户关闭正在挂载的实例 |
| `rclone` PID 25360 | 正在运行，RC 端口 5574 | 是**活体验证目标**：可用来验证握手、`config/paths`、`rc/list` 特性探测，无需自己造环境 |
| `cargo 1.99.0` / `rustc 1.99.0` | 可用 | — |
| `rustup` targets | `x86_64-pc-windows-msvc`、`wasm32-wasip1` | 插件与主程序都能构建 |
| 既有产物 | `target\debug\rclone-gui.exe` 171.6 MB / `target\release\…` 121.3 MB / `imagehost.wasm` 7.6 MB(debug)、0.4 MB(release) | 体积印证「内嵌 rclone + WASM」现状 |

### 1.3 内嵌 rclone 资产（阶段 7 的清理对象，现在**不动**）

| 路径 | 大小 | 说明 |
|---|---|---|
| `vendor/rclone.exe` | 82.8 MB（86,797,824 B） | 真实二进制，Git LFS 管理（`.gitattributes` 已就绪，未提交） |
| `vendor/rclone.zip` | 30.1 MB | 已加入 `.gitignore` |
| `vendor/rclone-extract/` | — | 已加入 `.gitignore` |

`build.rs` 用 `include_bytes!` 把它**编译进 exe**；运行期 `extract_embedded_rclone()` 再释放到 `%TEMP%\rclone-gui-embedded\rclone.exe` 并作为子进程启动。

### 1.4 用户数据（迁移 / 回滚的依据，禁止直接删除）

| 路径 | 内容 |
|---|---|
| `%APPDATA%\rclone-gui\config.json` | 735 B，含 `rclone.mode/port/user/pass/url`、`auto_mounts`、`auto_serves`、`serve_names`、`path_history`、`plugin_enabled`、`http`、主题、`bw_*` |
| `%APPDATA%\rclone-gui\config.json.bak` | 368 B（旧版备份） |
| `%APPDATA%\rclone-gui\rcd.log` | 496 KB（托管 rclone 日志，可能含远端路径，**不含** RC 密码） |
| `%APPDATA%\rclone-gui\plugins\imagehost.wasm` | 7.3 MB（内置插件释放副本） |
| `%APPDATA%\rclone-gui\plugins\imagehost\kv.json` | 406 B（图床索引） |
| `%APPDATA%\rclone-gui\cache\default\*.png` | 图床直链缓存 |
| `%APPDATA%\rclone-gui\tmp\` | 上传中转 |
| `%APPDATA%\rclone\rclone.conf` | 138 B，**真实 rclone 配置**（当前仅 1 个 remote 级别的极简内容） |
| HKCU\…\Run `RcloneGUI` | 当前**不存在**（`autostart=false`），但 `set_autostart()` 会写它 |

> 便携模式：exe 同目录存在 `portable` 文件时，数据目录改为 `exe目录\data`（`config::data_dir()`）。

---

## 2. 目标 vs 现状：差距矩阵

图例：✅ 已符合 · ⚠️ 部分符合 · ❌ 不符合 / 未实现

| # | 目标要求（AGENTS） | 现状 | 判定 | 归属阶段 |
|---|---|---|---|---|
| 1 | 产品名 RCGUI；内部名可分批迁移 | 全部命名 `rclone-gui` / `rclone_client` / `plugin_host`，UI 文案「Rclone GUI」 | ❌ | 7 |
| 2 | 不内嵌/释放 rclone | `build.rs` 嵌入 82.8 MB + `extract_embedded_rclone()` 释放到 `%TEMP%` | ❌ | 2(解耦)/7(删) |
| 3 | 不自动启动/停止 rclone | 启动即 `start_managed()` 拉起 `rclone rcd`；退出 `core/quit` + `child.kill()` | ❌ | 2 |
| 4 | 只连接用户显式选择的端点 | 端点来自 `config.json`，**无连接界面**；`find_reusable()` 会**主动扫描 `port..port+10` 并用凭据逐个探测** | ❌ | 3 |
| 5 | 不推测「有 rclone.exe 进程 = 有 RC」 | 逻辑上未做进程探测（好）；但 `discover_exe()` 遍历 PATH/常见路径自动选 exe | ⚠️ | 3 |
| 6 | 握手：版本 + 实际配置路径 + 特性，再读远程 | 只调 `core/version`；`detect_config_path()` 靠**子进程** `rclone config file`，且**从未被调用**（死代码）；无特性探测 | ❌ | 3 |
| 7 | 离线可用，保留可查看/编辑/导入/导出的本地档案 | 无任何本地 remote 副本；断连时 UI 保留上次快照但**无 stale 标记**，无法编辑/导入/导出 | ❌ | 3/4 |
| 8 | 多连接/档案可保存（首版可只激活一个） | 单一 `RcloneCfg`，无档案概念 | ❌ | 4 |
| 9 | remote 配置副本、期望挂载/服务配置、运行态**三者分离** | 期望态 = `auto_mounts/auto_serves`（有，但字段少）；remote 只有运行态；UI 运行态与预设**混在同一列表页** | ⚠️ | 4 |
| 10 | 不把运行态当可重建配置 | 已正确：`MountInfo` 只解析 `mountPoint`/`fs`，预设参数独立保存 | ✅ | — |
| 11 | 数据目录自有 + 旧目录迁移 + 回滚 | 目录 `%APPDATA%\rclone-gui` 即旧目录本身；无迁移、无回滚路径、无目录版本号 | ❌ | 4 |
| 12 | 版本化单文件 `rcgui.conf` 导入导出 + 预览 + 备份 + 敏感数据提示 | **完全没有** | ❌ | 5 |
| 13 | 不静默覆盖正在使用的 rclone 配置 | 修改 remote 走 `config/create|update|delete`（✅ 合规）；但**无备份**，且 `config_dump` 已把含密钥的完整配置读进内存 | ⚠️ | 5 |
| 14 | 启动脚本：显式生成、只连已运行实例、不启停 rclone | 无脚本生成；「自动恢复」由 `startup_flow()` 在应用内直接执行预设 | ⚠️ | 6 |
| 15 | 不按文件名前缀发现/删除脚本；用清单管理 | 无相关代码 | ✅(空) | 6 |
| 16 | 后台轮询只在窗口可见/前台；最小化暂停 | 1s `interval` **无条件** tick；`poll_stats` 每 1s、`apply_global_limit` 每 2s、`build_tasks` 每 10s，与窗口可见性无关 | ❌ | 3/6 |
| 17 | 同类型轮询不重叠；陈旧响应不得覆盖新状态 | `tick_busy` 防重入 + 30s 超时（✅）；但只有 `FilesRefresh{seq}` 做了序号；挂载/服务/远程列表无版本号 | ⚠️ | 3 |
| 18 | 只轮询轻量状态，不刷新文件列表/探测后端 | ✅ 未轮询文件列表；但 `apply_global_limit` 每 2s 调 `core/bwlimit`（写操作，非幂等读） | ⚠️ | 3 |
| 19 | 错误分类：连接/RC API/WinFsp/盘符占用/后端慢 | 挂载侧已分（`report_mount_not_visible`、盘符占用检测）；连接/API 侧统一 `toast("rclone 连接失败: {e}")` | ⚠️ | 3 |
| 20 | 保留旧功能可构建（文件、传输、插件、内嵌 HTTP） | 全部存在且可构建 | ✅ | — |
| 21 | 不调用 `core/quit`、不杀进程、不静默注册开机启动 | `Cmd::Quit` 里三件事全做了：`core_quit` + `child.kill()` + `set_autostart()` 写 Run 键 | ❌ | 2/7 |

**结论**：现状是一个**「托管式」桌面壳 + RC 客户端**。产品核心（连接、离线档案、导入导出）几乎为零，而现有功能（文件、传输、插件、图床、内嵌 HTTP）庞大且必须保留可构建。因此不能重写，只能「先解耦、再在旁边长出新的核心，最后拆掉旧的」。

---

## 3. 依赖地图

### 3.1 Crate 依赖

```
rclone-gui (crates/app)           bin, 5023 行 + 2883 行 Slint
 ├─ rclone_client (crates/rclone)  进程管理(proc.rs 450) + RC 客户端(rc.rs 357) + 类型(types.rs 144)
 ├─ plugin_host   (crates/plugin)  WASM 宿主(lib.rs 243) + 包管理(package.rs 810) + ZIP(zip.rs 512)
 ├─ slint 1 (renderer-software, winit-030)
 ├─ axum 0.8 + tower-http         内嵌 HTTP（http.rs 389）
 ├─ tray-icon 0.14 / arboard 3 / rfd 0.15 / open 5 / image 0.25
 └─ winreg 0.52 / windows 0.58    注册表、单实例互斥、托盘 HWND

imagehost-plugin (plugins/imagehost)  cdylib→wasm32-wasip1, 928 行, 无工作区依赖
```

关键点：`plugin_host` 与 `rclone_client` **互不依赖**，`rclone_client` 目前把「进程管理」和「RC 客户端」焊在同一个 crate 里 —— 这正是阶段 2 要拆的缝。

### 3.2 运行时拓扑（现）

```
主线程            Slint 事件循环（MainWindow / 12 个 global / 60+ callback）
  │  mpsc::UnboundedSender<Cmd>（62 种命令）
  ▼
后台线程          tokio multi-thread(2 workers)
  ├─ backend_main(): 启动/attach rclone → 首次刷新 → startup_flow → 内嵌 HTTP → IPC → select 主循环
  │    ├─ 1s interval tick  → poll_stats / build_tasks(10s) / apply_global_limit(2s) / 主题跟随(3s)
  │    ├─ rx.recv()         → handle_cmd()（1903 行 match，58 个分支；除 Quit 外全部 tokio::spawn）
  │    └─ http_restart.notified() → spawn_http / stop_http
  └─ 插件调用（wasmtime，blocking 池）
独立线程           tray-icon 事件循环
子进程            rclone rcd（托管模式）/ rclone cat（直链回源）/ 插件 entry_file(msiexec|cmd|powershell)
监听端口          RC 5574(托管) · 内嵌 HTTP 5273 · 单实例 IPC 52799
```

### 3.3 耦合热点（改造工作量刻度）

| 指标 | 数量 | 位置 |
|---|---|---|
| `Ctx` 上的 `mgr` 引用 | 45 处 | `main.rs` 45 / `http.rs` 1 / `kv.rs` 2（其中 `mgr.client.read()` 36 处） |
| `client.<method>()` 调用 | 90 处 | `main.rs`；涉及 29 个 RC 方法 |
| `Cmd` 枚举变体 | 62 | `main.rs:26-96` |
| `match` 分支（handle_cmd） | 58 | `main.rs:1911-3813` |
| Slint `global` / `component` | 12 global / 10 视图 / 13 基础组件 | `main.slint` |
| Slint `callback` | 254 个匹配行 → 约 130 个不同 callback | `main.slint` + `wire_callbacks` 544 行 |

> 这 4 个数字决定拆分顺序：**先把「RC 调用点」从 `Ctx.mgr.client` 收敛到一个 `Connection` 门面**，之后 `handle_cmd` 的机械拆分才安全（不用重复改 90 个调用点）。

---

## 4. 进程管理入口全清单（AGENTS §7.1 要求「每一个进程管理入口」）

| # | 位置 | 行为 | 备注 / 迁移去向 |
|---|---|---|---|
| P1 | `crates/app/build.rs:18-35` | `include_bytes!` 把 `vendor/rclone.exe` 编进 exe | 阶段 7 删除；阶段 2 起用 feature 关闭 |
| P2 | `main.rs:1044-1070` `extract_embedded_rclone()` | 释放 82.8 MB 到 `%TEMP%\rclone-gui-embedded` | 阶段 7 删除 |
| P3 | `main.rs:1091-1102`（`main()`） | 内嵌优先 → 否则 `discover_exe()` 自动找 exe | 阶段 3 改为「用户选择端点」 |
| P4 | `main.rs:1263-1300`（`backend_main`） | `mode==0` → `start_managed()`；`mode==1` → `attach()` | 阶段 2 拆成 `ProcessSupervisor`（legacy）与 `Connection::connect` |
| P5 | `main.rs:1913-1952`（`Cmd::Quit`） | 卸载全部挂载 → 停全部 serve → `core/quit`(5s 超时) → 轮询 `is_managed_running` → `child.kill()` | 阶段 2 移除 kill/quit；改为「断开连接」 |
| P6 | `main.rs:1472`（`backend_main` 尾部） | `mgr.stop()`（loop 退出后兜底 kill） | 同上 |
| P7 | `main.rs:3755`（`Cmd::SettingsTest`） | `extract_embedded_rclone()` 取路径做「测试连接」 | 阶段 7 删除 |
| P8 | `crates/rclone/src/proc.rs:195-292` `start_managed()` | 复用残留实例 → 端口被占换端口 → 打开日志 → `rclone rcd` 子进程 → 20s 就绪轮询 | 阶段 2 整块隔离为 legacy 模块 |
| P9 | `proc.rs:294-298` `stop()` | `Child::kill()` | 同上 |
| P10 | `proc.rs:300-306` `is_managed_running()` | `try_wait()` | 同上 |
| P11 | `proc.rs:168-189` `detect_config_path()` | 子进程 `rclone config file` 解析路径 | **死代码**；阶段 3 用 `config/paths` 取代 |
| P12 | `proc.rs:89-116` `port_in_use()` / `pick_free_port()` / `find_reusable()` | 扫描本机端口并凭据探测 | 违反 §3；阶段 3 删除 |
| P13 | `proc.rs:320-348` `cat_to_file()` | `rclone cat` 子进程流式写缓存（图床直链回源） | 插件侧依赖；阶段 7 换 `operations/cat`/HTTP |
| P14 | `main.rs:1238-1246` | `CreateMutexW("rclone-gui-singleton")` 单实例 | 与 rclone 生命周期无关，保留 |
| P15 | `main.rs:283-304` `set_autostart()` | 写 HKCU Run `RcloneGUI` | 属「应用自身开机启动」，非 rclone 生命周期；阶段 6 需与「恢复脚本」严格区分 |
| P16 | `plugin/src/package.rs:495-520` | 插件 entry 执行：`msiexec /qn`、`.exe`、`cmd /c`、`powershell` | 外部代码执行面；`requires_admin` 未实现（无 UAC 提权） |

**RC 写操作/退出类调用**：`core_quit`(P5)、`core/bwlimit`（每 2s）、`options/set`、`config/create|update|delete`、`mount/mount|unmount`、`serve/start|stop`、`sync/*`。阶段 2 只需切断 `core_quit` 与 `child.kill()`，其余保留（用户显式操作触发）。

---

## 5. 数据来源清单

| 来源 | 当前实现 | 目标处理 |
|---|---|---|
| 已配置 remote | `config/listremotes`(名字) + `config/dump`(类型/usage/参数)，仅内存缓存 `remotes_cache` | 增加 RCGUI 自有副本（阶段 4），运行态只读展示 |
| 后端参数模板 | `config/providers`，缓存于 `providers_cache`，**不失效** | 保留，加失效策略 |
| 运行中的挂载 | `mount/listmounts`（旧名 `mount/list` 回退），`parse_mounts` 宽容解析 | 保留，与预设分列表呈现 |
| 运行中的 serve | `serve/list` + 本地 `serve_names` 命名映射 | 保留 |
| 期望挂载/服务 | `AppConfig.auto_mounts` / `auto_serves`（`enabled` 开关），含 `remote/sub_path/point/ro/cache`、`stype/addr/user/pass` | 升级为版本化 profile（阶段 4） |
| 传输/统计 | `core/stats`、`core/transferred`、`job/list|status|stop` | 保留（注意 §6 的轮询约束） |
| 插件启用表 / KV | `config.json.plugin_enabled` + `plugins/<id>/kv.json` | 保留 |
| rclone 真实配置路径 | **无**（`detect_config_path` 死代码） | 阶段 3 用 `config/paths` |

### 已实测的 rclone 能力面（活体验证，端口 5574）

- 版本：`v1.75.2-extra-11-g066443b42`（gulp79/rclone-extra，`goTags=cmount`，Windows 挂载必需）
- `config/paths` → `config: ***REMOVED***\AppData\Roaming\rclone\rclone.conf`（**无需子进程**）
- `rc/list` 共 **95** 个方法；本应用需要的全部存在：
  `config/{providers,dump,paths,create,update,delete,setpath}`、`mount/{mount,unmount,listmounts}`、`serve/{start,stop,list}`、`core/{quit,stats,transferred,bwlimit}`、`options/{get,set}`、`operations/{copyfile,movefile}`、`job/{list,status,stop}`

→ 阶段 3 的握手可直接用 `core/version` + `config/paths` + `rc/list`（或 `core/version` 的 `decomposed` 做版本门槛），**完全不需要启动子进程**。

---

## 6. 构建链现状

```
scripts/fetch-rclone.ps1            git lfs pull vendor/rclone.exe（校验 86,797,824 B + SHA256）
        ▼
cargo build -p imagehost-plugin --target wasm32-wasip1   必须第 1 步
        ▼  target/wasm32-wasip1/<profile>/imagehost.wasm
cargo build -p rclone-gui          第 2 步；build.rs 去上面找 wasm
        ├─ slint_build::compile("ui/main.slint")           失败即中断
        ├─ OUT_DIR/embedded_rclone.rs  ← vendor/rclone.exe → RCLONE_BYTES
        └─ cargo:rustc-env=IMAGEHOST_WASM=<abs> → main.rs:1225 include_bytes!
```

风险（阶段 7 前必须记住）：
1. `include_bytes!(concat!(env!("IMAGEHOST_WASM")))` 在 `main.rs:1225` 是**硬编码**：`IMAGEHOST_WASM` 为空时**编译失败**，不是降级。`build.rs` 的 `cargo:warning` 提示「未找到」其实已经来不及。
2. `build.rs` 在 `../` 与 `../../` 下找 wasm，说明它假定 `CARGO_MANIFEST_DIR` 深度；换 target 目录不影响，但换 workspace 结构会断。
3. `scripts/build.ps1` 带 UTF-8 BOM（进度总结已注明「勿丢 BOM」，否则中文注释乱码）。
4. 删除内嵌 rclone **不得**顺带删除 wasm 构建步骤（AGENTS §8 明确警告）。

---

## 7. 拆分计划（`main.rs` / `main.slint`）

`main.rs` 5023 行、`main.slint` 2883 行是本项目最大的两个单文件。目标结构（**增量迁移，每一步都可编译**）：

### 7.1 `crates/app/src/main.rs` → 模块

| 新模块 | 行数 | 现有行号 | 内容 |
|---|---|---|---|
| `cmd.rs` | ~80 | 23-96 | `Cmd` 枚举（62 变体）与派发约定 |
| `state.rs` | ~60 | 98-136 | `Ctx` → 拆为 `AppState`（缓存/配置）+ `Services`（host/store/http） |
| `ui_bridge.rs` | ~150 | 140-185, 373-485, 857-872, 996-1028 | toast、`SearchDebounce`、`read_ui`、列表筛选应用、provider 表单桥接 |
| `naming.rs` | ~70 | 186-248 | serve 自动命名、路径历史（`push_history`/`set_history`） |
| `platform.rs` | ~95 | 249-338 | 注册表主题、`winreg` 开机启动、WinFsp 探测、剪贴板 |
| `fmt.rs` | ~60 | 312-323, 506-546, 1034-1043 | 字节/速率/时间格式化（与 `rclone_client::types` 的 `fmt_bytes` 重复，需归一） |
| `fs_path.rs` | ~150 | 340-372, 873-995, 1706-1811 | fs/display 互转、mount_key/归一化、就绪等待、盘符占用 |
| `views/dashboard.rs` | ~70 | 1953-1983, 1871-1908 | 仪表盘刷新、统计轮询 |
| `views/files.rs` | ~330 | 1984-2287 + 4208-4224 | 文件页 8 个命令 |
| `views/mounts.rs` | ~300 | 2288-2578 | 挂载页 6 个命令（`MountAdd` 单块 147 行） |
| `views/remotes.rs` | ~300 | 2752-3042 | 远程页 7 个命令 + provider 表单（723-856） |
| `views/serves.rs` | ~330 | 3043-3368 | 服务页 8 个命令（`ServeStart` 单块 136 行） |
| `views/transfers.rs` | ~190 | 3369-3488 + 547-722 | 任务列表、全局限速、`options/*` |
| `views/settings.rs` | ~70 | 3748-3813 | 设置保存/测试、`copy-text`、退出 |
| `plugins.rs` | ~500 | 3489-3594, 3814-4207 | 插件扫描/安装/启用、图床配置 |
| `wiring.rs` | ~560 | 4227-4770 | `wire_callbacks`（按视图再分 3~4 个文件） |
| `tray.rs` | ~100 | 4773-4869 | 托盘线程与图标 |
| `startup.rs` | ~230 | 1478-1705 | `startup_flow`（阶段 6 重写为「恢复计划」） |
| `app.rs` | ~180 | 1074-1232, 1234-1253 | `main()`、单实例互斥与 IPC 唤醒、`embed_plugins` |
| `legacy_proc.rs` | ~40 | 1044-1070, 1091-1102 | 内嵌释放（阶段 7 整文件删除） |

拆分顺序建议：`fmt/fs_path/platform/ui_bridge`（叶子，零风险）→ `views/*`（纯移动）→ `wiring`（按视图切）→ `startup`/`app`。

### 7.2 `main.slint` → 文件

| 新文件 | 行数 | 现有行号 |
|---|---|---|
| `ui/globals.slint` | ~180 | 1-301（12 个 global + 数据结构） |
| `ui/widgets.slint` | ~350 | 302-602（13 个基础组件） |
| `ui/views/dashboard.slint` | 77 | 603-679 |
| `ui/views/files.slint` | 221 | 680-900 |
| `ui/views/remotes.slint` | 258 | 901-1158 |
| `ui/views/mounts.slint` | 244 | 1159-1402 |
| `ui/views/serves.slint` | 251 | 1403-1653 |
| `ui/views/transfers.slint` | 144 | 1675-1818 |
| `ui/views/plugins.slint` | 376 | 1819-2194 |
| `ui/views/settings.slint` | 155 | 2195-2349 |
| `ui/shell.slint` | ~435 | 2350-2883（标题栏 + MainWindow 壳） |

Slint 的 `export global` 是跨文件共享的，把 global 收进 `globals.slint` 后各视图只需 `import`，**无需改动 callback 名称**，风险低。

---

## 8. 阶段 2-7 执行计划（含验收点）

> 原则：每阶段结束**可编译、可运行、旧功能不退化**；不一次性重写。

### 阶段 2 · 解耦 RC 客户端（本次盘点后的下一步）

- 新建 `crates/rc/`（`rcgui-rc`）：只含 `RcClient`（连接/鉴权/超时/方法封装）+ `Connection`（端点、凭据、状态机 `Disconnected/Connecting/Connected/Stale`、最近快照）。**零 `std::process`**。
- 在 `app` 里引入 `Connection`，把 90 处 `mgr.client.read().await.clone()` 逐步改为 `state.conn.client()`；`RcloneManager` 退化为 `ProcessSupervisor`（legacy），仅 `mode==0` 路径调用。
- 迁移全部调用方后，`proc.rs` 才允许被 feature-gate。
- 验收：`cargo build` 通过；用**外部已运行**的 rclone（PID 25360）走完整 CRUD；不产生任何 `rclone` 子进程。

### 阶段 3 · 连接状态与离线 UI

- 握手：`core/version` → 版本门槛（`decomposed` ≥ 1.60）→ `config/paths`（真实配置路径）→ `rc/list`（特性表）。缺失 API 明确报「不支持」而非通用失败。
- 连接页：地址/端口/用户名/密码/是否记住；默认 `http://127.0.0.1:5572`；**禁止**端口扫描（删除 `find_reusable`/`pick_free_port`）。
- 离线模式：启动不连接，UI 可用；`Stale` 标记保留上次快照；重试/重连按钮。
- 轮询：仅在窗口可见时按 2s/5s 分级；最小化/托盘隐藏即暂停；每类请求单飞 + 序号防陈旧覆盖。
- 验收：无 rclone 进程时能启动、能看/改档案；连接后 3s 内显示 live 状态；最小化 30s 后 `rcd.log` 无新增请求。

### 阶段 4 · 数据模型与持久化

- `rcgui-profile` 模型：`schema_version`、`profile{id,name}`、`remotes[]`、`mounts[]`、`serves[]`、`connections[]`。
- 目录 `%APPDATA%\RCGUI`（或用户指定），迁移旧目录 = **复制 + 保留旧目录 + `migration.json` 回滚点**。
- 运行态模型与期望态模型类型上分离（现有 `MountInfo` 保留为 runtime DTO）。
- 验收：旧 `config.json` 自动迁移且可回滚；断开后本地档案完整可编辑。

### 阶段 5 · 导入/导出

- `rcgui.conf`：JSON（`application/vnd.rcgui.conf+json`）单文件，含 `schema_version` + 各段 + 可选 `script_manifest`；或 zip 容器（若含脚本，必须做路径穿越/重名/超大/校验防护）。
- 流程：解析 → 校验 → **预览**（档案名、remote 列表、挂载/服务数量、目标配置路径、合并差异）→ 应用（先备份 `rclone.conf`）→ 失败可回滚。
- 敏感数据：导出时二次确认 + 明示含 OAuth token/密码；`obscure` 不是加密，需明说；默认不写日志。
- 验收：导入到「rclone 正在使用该配置」时**不静默覆盖**，给出备份路径与「需手动重启 rclone」提示。

### 阶段 6 · 恢复脚本

- 由结构化档案生成 PowerShell：参数数组/`ConvertTo-Json` 序列化，**不拼字符串**；先 `core/version` 健康检查，端点不通即 `exit 1`；**不**启动/终止 rclone。
- 生成前预览脚本全文与目标路径；写入 `%LOCALAPPDATA%\RCGUI\scripts\` + `manifest.json`（记录文件名/版本/哈希）；清理只按清单。
- 与「应用开机启动」（HKCU Run）严格分离；不自动建计划任务。
- 验收：脚本对已断开端点 `exit 1`、不改任何进程；清单可完整撤销生成物。

### 阶段 7 · 清理与文档

- 删除：`build.rs` 内嵌段、`extract_embedded_rclone`、`vendor/`（先确认零引用）、`scripts/fetch-rclone.ps1`、`proc.rs` 剩余进程管理、`find_reusable`、`core_quit`/`kill` 调用。
- **保留** WASM 插件构建链（`IMAGEHOST_WASM`），改为「找不到 wasm 时优雅降级」而不是编译失败。
- 重命名：产品名 RCGUI、`rclone-gui` → `rcgui`（crate/exe/数据目录分批）；README/构建文档/数据迁移说明同步。
- 验收：AGENTS §9 六条全部满足。

---

## 9. 风险清单

| 等级 | 风险 | 缓解 |
|---|---|---|
| 高 | **构建被运行中实例锁住**（`target\debug\rclone-gui.exe` 被 PID 22616 占用，proj 经验：`os error 5`） | 用 `CARGO_TARGET_DIR=target-rcgui` 旁路构建；不改用户正在用的产物 |
| 高 | `main.rs` 5023 行 + 90 处 RC 调用点，机械拆分极易引入行为漂移 | 先做「调用点收敛」（阶段 2 第一步），再拆文件；每步 `cargo check` |
| 高 | `config_dump` 已把含 OAuth token 的完整配置读进内存，任何日志/崩溃转储都会泄露 | 新增 `Secret` newtype 包裹凭据；`{:?}` 手写脱敏；阶段 5 前禁止把 dump 落盘 |
| 中 | 移除内嵌 rclone 时误删 wasm 构建步骤 → 编译失败 | `IMAGEHOST_WASM` 改为可选（空则跳过内置插件并提示），先改 build.rs 再删 vendor |
| 中 | 用户数据目录改名导致「配置丢失」观感 | 迁移 = 复制 + 保留 + `-rollback` 目录 + 首启提示；便携模式同样处理 |
| 中 | 停用托管模式后，现有用户「打开即用」体验消失 | 连接页保留「一键连接本机 5574」；离线可先看档案；给出 rclone `--rc` 启动指引（不代劳） |
| 低 | Slint 拆文件后 `global` 单例仍工作，但循环 import 会报错 | `globals`/`widgets` 只被视图引用，视图之间不互相 import |
| 低 | 托盘/单实例/IPC 与重构交叉 | 归入 `platform.rs`/`app.rs`，最后动 |

---

## 10. 需要确认的最小问题（不阻塞继续盘点）

1. **阶段 2 过渡策略**：托管模式（自动 `rclone rcd`）是「保留为高级开关（默认关）」还是「阶段 2 直接移除」？——影响 P3/P4/P8/P9 的删除时机，也影响老用户升级体验。
2. **数据目录命名**：新目录用 `%APPDATA%\RCGUI` 还是 `%APPDATA%\rcdeck`？（阶段 4 才落地，但模型/路径常量宜先定）
3. **本轮交付范围**：是否希望我按阶段 2 → 3 逐段实施并在每段结束提交一份阶段报告（期间需要旁路构建验证），还是先只把阶段 1 的拆分计划落到代码骨架。

---

## 附：本次盘点未改动任何文件

- 唯一新增文件：`docs/refactor/phase-1-inventory.md`（本文）。
- `git status` 与盘点前一致（除本文）。
- 未删除 `vendor/`、未触碰 `%APPDATA%` 数据、未杀进程、未运行构建或测试。
