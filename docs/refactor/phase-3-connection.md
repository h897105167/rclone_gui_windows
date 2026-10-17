# RCGUI 重构 · 阶段 3：连接状态与离线 UI

> 依据：`AGENTS.md` §3（连接与数据源）、§6（刷新与 UI 行为）、§7.3。
> 前置：[phase-1-inventory.md](./phase-1-inventory.md) · [phase-2-decoupling.md](./phase-2-decoupling.md)
> 完成时间：2026-10-17 · 基线 HEAD `a463f90`（用户未提交改动全部保留）

---

## 1. 判定标准与本阶段结论

| 判定项 | 结果 |
|---|---|
| 连接是否由用户显式选择的端点决定 | ✅ 设置页「rclone 连接」区可填 模式/地址/端口/用户/密码，新增「连接 / 断开 / 重新握手」按钮 |
| 是否停止扫描本机端口 | ✅ 删除 `find_reusable()`（原会遍历 `port..port+10` 逐个用凭据探测 `core/version`） |
| 握手是否覆盖版本 / 实际配置路径 / 支持特性 | ✅ `core/version` → `config/paths` → `rc/list`（18 项能力表，10 必需 / 8 可选） |
| 缺失 API 与版本过旧是否明确报告 | ✅ `RcErrorKind::{Unsupported,Connect,Auth,Api,Decode,Timeout}` 各有 `label()`/`hint()`；缺能力单独列出 |
| 离线是否可用 | ✅ 连接失败只提示、不阻断；断开后**零 RC 请求**，界面保留上次快照并标 `Stale` |
| 轮询是否只在可见且前台时进行 | ✅ UI 线程 500ms 采样 `is_visible && !is_minimized` → 后台读 `AtomicBool` 门控 |
| 同类请求是否单飞、旧响应是否可能覆盖新状态 | ✅ 挂载/服务/远程三路各一把 `GateGuard`（Drop 自动复位）+ 未连接直接跳过 |
| 是否改变用户可见行为 | ⚠️ 仅新增（连接按钮、状态区、断开语义）；原有自动连接与挂载预设流程保持不变 |

---

## 2. 改动清单

### 2.1 `rcgui-rc`（纯 RC，无进程）

| 文件 | 行数 | 内容 |
|---|---|---|
| `src/handshake.rs` | 235 | **新增**。`Handshake::perform()` 三步握手；`METHOD_REQS` 能力表；`HandshakeError` 语义（仅 `core/version` 失败才 `Err`）；`is_compatible/supports/config_path/summary/missing_text` |
| `src/conn.rs` | 223 | `RcState` 增加 `live: AtomicBool` 同步镜像与 `is_live()`：轮询热路径不 await 也能判断该不该发请求 |
| `src/lib.rs` | 26 | 导出 `handshake` 模块与 `MIN_MAJOR/MIN_MINOR/MethodReq` |

### 2.2 `rclone_client`（legacy 进程管理）

| 文件 | 改动 |
|---|---|
| `src/proc.rs` | **删除 `find_reusable()`** 及其调用（约 -24 行，附注释说明为什么不能这么做）；`pick_free_port()` 保留并注明「只为自己即将启动的子进程挑端口，不向任何服务发请求」 |

### 2.3 `app`

| 文件 | 改动 |
|---|---|
| `src/main.rs` | 新增「连接与握手」区块（`ConnView`/`ConnOutcome`/`ConnAction`/`apply_conn_view`/`snapshot_view`/`handshake_only`/`connect_report`/`conn_action`/`disconnect_now`）；`Ctx` 增加 `visible`/`conn_busy`/`gate_{mounts,serves,remotes}`；`backend_main` 第 1 步改为「连接 → 握手 → 分类汇报」；tick 增加可见性门控与恢复补刷；三个刷新命令加单飞闸门与未连接短路；`SettingsTest` 改为完整握手；新增 `Cmd::{Connect,Disconnect,Reconnect}` 及实现；UI 线程可见性采样定时器 |
| `ui/main.slint` | `SettingsG` 增加 9 个 `conn-*` 属性与 3 个连接回调；`DashboardG` 增加 `stale`；连接卡片内新增状态行（状态点/端点/版本/配置文件/上次刷新/错误）+ 三按钮；仪表盘「rclone 状态」区分 运行中 / 已断开（显示上次快照）/ 未连接 |
| `src/kv.rs` · `Cargo.toml` | 沿用阶段 2；本阶段未改 |

---

## 3. 握手设计

```
connect_report()
  ├─ 托管模式 且 子进程仍存活 → 只重新握手（不重启进程，避免连带卸载挂载）
  ├─ 托管模式 → start_managed()          （拉起 rcd，端口被占时换端口并明确提示）
  ├─ 连接模式 → attach(url, user, pass)  （只连用户填的端点）
  └─ handshake_only()
        ├─ core/version   → 版本 + decomposed（< 1.60 判为「版本过旧」）
        ├─ config/paths   → 真实 rclone.conf 路径（失败仅告警，不回退去猜）
        └─ rc/list        → 方法表 → 与 METHOD_REQS 求差
              ├─ 必需缺失（10 项：config/listremotes|create|update|delete、mount/listmounts(或 mount/list)|mount|unmount、serve/list|start|stop）
              │     → status = 「已连接（能力受限）」+ 逐项列出
              └─ 可选缺失（8 项：config/paths|providers、options/get|set、core/bwlimit|transferred、job/stop、operations/about）
                    → 仅作为告警，不阻断
```

只有 `core/version` 失败才返回 `Err(RcError)`，因此「连不上 / 鉴权失败」是硬失败，其余都能降级使用——老版本 rclone 仍然可读远程、可挂载。

`RcErrorKind` 六分类在 UI 上体现为不同文案与建议，例如：

| 分类 | 展示 | 建议 |
|---|---|---|
| `Connect` | 无法连接 | 确认 rclone 已用 `--rc` 启动，检查地址与端口 |
| `Auth` | 鉴权失败 | 检查 `--rc-user` / `--rc-pass` |
| `Unsupported` | rclone 不支持该接口 | 升级 rclone |
| `Timeout` | 请求超时 | 远程后端可能较慢或已卡住 |

---

## 4. 轮询纪律（§6）

| 要求 | 实现 |
|---|---|
| 仅窗口可见且前台时轮询 | UI 线程 `Timer`（500ms）采样 `with_winit_window(|w| w.is_visible() && !w.is_minimized())` → `AtomicBool`；tick 首行检查，不可见即 `continue`（**不产生任何 RC 请求**，但主题跟随等本地逻辑仍运行） |
| 恢复时立即刷新 | `was_visible` 迁移检测：不可见→可见时立刻派发 `RefreshDashboard` + `MountsRefresh` + `ServesRefresh` |
| 同类轮询不重叠 | `GateGuard`（`Drop` 自动复位）分别守护 挂载/服务/远程 三路；重叠的那次直接丢弃 |
| 旧响应不得覆盖新状态 | 单飞使同类请求串行化，故不存在「旧响应后到」；文件页仍用 `FilesRefresh{seq}` 序号 |
| 只轮询轻量状态 | 1s tick：`core/stats`（轻）；挂载计数降到 5s 一次；`core/transferred` 10s；限速仅在配置值变化时下发（内部已判重） |
| 未连接时不空转 | `RcState::is_live()` 同步门控：断开后 tick 不轮询、三个刷新命令直接短路，**保留上次快照** |
| 更新时间戳 | 已连接时每 5s 刷新「上次刷新 hh:mm:ss」 |

---

## 5. 离线与「陈旧」语义

| 场景 | 表现 |
|---|---|
| 启动时连不上 | 窗口照常打开；仪表盘显示「未连接」；设置页连接区显示失败原因 + 建议；可继续填地址后点「连接 / 握手」 |
| 运行中断开（点「断开」） | `ConnStatus::Stale`：仪表盘显示「已断开 · 显示上次快照」（黄色），设置页状态点变黄；不再发任何 RC 请求；列表内容保留 |
| 重新握手 | 只跑握手，不启停进程；成功即恢复 `Connected` 并清空错误 |
| 托管模式下「断开 → 连接」 | 检测到子进程仍存活 → 只重新握手，**不会重启进程**（重启会连带卸载挂载、停止 serve） |

---

## 6. 验证

> 按既定口径：只做 `cargo check`，不构建 exe、不跑测试。

| 命令 | 结果 |
|---|---|
| `cargo check -p rcgui-rc -p rclone_client -p rclone-gui --all-targets` | **exit 0** |
| `cargo check --workspace` | **exit 0** |
| 警告 | 仅 2 条既有警告（内嵌 rclone 82 MB、插件 `encodeURIComponent` 命名） |

`cargo check` 会执行 `build.rs → slint_build::compile`，因此 **`.slint` 改动（9 个新属性、3 个新回调、状态行布局）已被真正编译验证**，不是「看起来对」。

不变量断言（grep 实测）：

| 断言 | 结果 |
|---|---|
| `find_reusable` 代码残留 | 0（仅剩说明性注释） |
| `port..port+N` 凭据扫描 | 0（`pick_free_port` 只做本地 bind 测试） |
| `Handshake::perform` 调用点 | 1（统一入口，启动与手动连接共用） |
| `rc.is_live()` 门控 | 5 处（tick、Dashboard、挂载、服务、远程） |
| `GateGuard` | 1 结构体 + 3 使用点 |
| 连接回调接线 | 3（`on_conn_connect` / `on_conn_disconnect` / `on_conn_reconnect`） |

---

## 7. 与阶段 2 计划的偏差（须知情）

| 计划 | 实际 | 原因 |
|---|---|---|
| 删除 `pick_free_port` 的隐式换端口 | **保留**，但仅在托管模式下、且必有 toast 提示 | 它不探测任何外部服务；托管模式仍是默认模式，直接报错会让默认配置的用户「打开即不可用」。彻底移除随托管模式一起在阶段 7 处理 |
| 启动即进入离线、由用户点「连接」 | **未改**：默认仍按 `mode=0` 自动连接 | 自动连接同时驱动「自动挂载/自动服务」预设；改为离线优先会改变现有用户的开机行为，属于产品边界决策（§1），应在阶段 7 与托管模式一并处理 |
| 连接页独立导航项 | **未新增**：落在设置页「rclone 连接」卡片内 | 用户选择「先以设置页连接区落地」 |

---

## 8. 风险与待办

| 等级 | 项 | 说明 |
|---|---|---|
| 高 | 仍会主动拉起 rclone（默认托管模式） | 与 §1「不自动启动 rclone」冲突，属阶段 7 范围；本阶段只保证「断开由用户显式触发且不杀进程」 |
| 中 | 仅静态验证 | 握手/门控逻辑未经运行期验证。首次运行时应重点看：连接失败文案是否符合分类、断开后 `rcd.log` 是否停止增长、最小化 1 分钟是否无新请求 |
| 中 | 端口回退后未持久化 | `start_managed` 换端口后 `cfg.rclone.port` 仍是旧值；已用 `RcState.endpoint()` 作为真实地址展示，配置值待阶段 4 的 profile 模型统一 |
| 低 | `RcState` 单端点 | 首版允许只激活一个端点（§3）；多档案在阶段 4 引入世代号后再扩展 |
| 低 | 断开后文件页仍可手动请求 | 文件页是运行态浏览，未加 `is_live` 短路（保留「手动刷新」语义）；如需要，阶段 4 一并处理 |

---

## 9. 下一阶段

**阶段 4 · 数据模型与持久化**：

1. 版本化 profile 模型：`schema_version` + `profile{id,name}` + `remotes[]` + `mounts[]` + `serves[]` + `connections[]`，落盘与运行态 DTO 类型分离。
2. 数据目录迁移：`%APPDATA%\rclone-gui` → 新目录（名称待定），**复制 + 保留旧目录 + `migration.json` 回滚点**，首启提示。
3. `mount/listmounts` 运行态与用户保存的挂载参数彻底分离（现有 `AutoMount/AutoServe` 升级为 profile 条目，补齐缺失字段）。
4. 「已保存 / 运行中 / 待应用 / 陈旧」四态在模型层可判定，UI 后续直接用。

验收：旧 `config.json` 自动迁移且可回滚；断开后本地档案完整可编辑；不会把运行态当成可重建配置。
