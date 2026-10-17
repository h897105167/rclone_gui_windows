# RCGUI 重构 · 阶段 2：解耦 RC 客户端

> 依据：`AGENTS.md` §7.2「Separate connection/authentication/API calls to an external endpoint
> from launching/stopping rclone child processes. Migrate callers before removing process-management code.」
> 阶段 1 盘点见 [phase-1-inventory.md](./phase-1-inventory.md)。
> 完成时间：2026-10-17 · 基线 HEAD `a463f90`（工作树含用户未提交改动，未回滚任何一处）。

---

## 1. 判定标准与本阶段结论

| 判定项 | 结果 |
|---|---|
| 「调用 RC API」是否可以在没有进程管理器的情况下完成 | ✅ `rcgui-rc` 对 `rclone_client` 零依赖 |
| `rcgui-rc` 内是否含任何进程启动/终止代码 | ✅ 0 处（`std::process` / `tokio::process` / `Command` / `kill` 均无） |
| 调用方是否已全部迁移到新接缝 | ✅ app 内 `mgr.client` 0 处；36 处 RC 入口统一走 `ctx.rc.client().await` |
| 进程管理代码是否**保留**（按指南「先迁移后删除」） | ✅ 完整保留，仅退居 legacy |
| 行为是否改变 | ❌ 无用户可见变化（等价替换，见 §5） |

**一句话**：RC 调用与 rclone 进程生命周期在 crate 和类型两个层面已经分开；进程管理仍在，但已不再是「用 RC 的必经之路」。

---

## 2. 改动清单

### 2.1 新增 crate `crates/rc`（包名 `rcgui-rc`）

| 文件 | 行数 | 作用 |
|---|---|---|
| `Cargo.toml` | 11 | 仅依赖 `reqwest`/`serde`/`serde_json`/`tokio`，**无进程相关依赖** |
| `src/lib.rs` | 26 | 模块聚合与 re-export |
| `src/error.rs` | 171 | `RcError` / `RcErrorKind` / `RcResult`：把「连不上 / 鉴权失败 / 不支持 / API 错误 / 解析失败 / 超时」分类 |
| `src/conn.rs` | 223 | `RcState`（连接状态持有者）、`Endpoint`、`ConnStatus{Disconnected,Connected,Stale}` |
| `src/client.rs` | 473 | `RcClient`：HTTP + 鉴权 + 方法封装（由 `crates/rclone/src/rc.rs` **`git mv` 迁入**，保留历史） |
| `src/types.rs` | 177 | RC DTO + `parse_mounts` + `fmt_*`（由 `crates/rclone/src/types.rs` 迁入），新增 `ConfigPaths`、`VersionInfo::at_least` |

### 2.2 修改

| 文件 | 改动 |
|---|---|
| `Cargo.toml`（workspace） | members 增加 `crates/rc` |
| `crates/rclone/Cargo.toml` | `+rcgui-rc`，`−reqwest`（HTTP 已不属于本 crate） |
| `crates/rclone/src/lib.rs` | 改为 **legacy 说明 + 过渡期 re-export**：`RcClient`/`RcState`/类型仍可从旧路径导入 |
| `crates/rclone/src/proc.rs` | `RcloneManager` 不再持有 `client: RwLock<Arc<RcClient>>`，改为持有 `rc: Arc<RcState>` 并在启动/连接成功后**发布**客户端；**删除 `detect_config_path`（子进程）**；失败路径写入 `set_disconnected` |
| `crates/rclone/examples/smoke.rs` | 适配新签名（`RcloneManager::new(log, rc)` + `rc.client()`） |
| `crates/app/Cargo.toml` | `+rcgui-rc` |
| `crates/app/src/main.rs` | `Ctx` 新增 `rc: Arc<RcState>`；36 处 `ctx.mgr.client.read().await.clone()` → `ctx.rc.client().await`；6 处 `timeout(...).and_then(|r| r)` 适配 `RcError→anyhow`；`caps()`/`spawn_http()` 构造改为传 `rc` |
| `crates/app/src/kv.rs` | 插件能力 `RealCaps` 不再依赖 `RcloneManager`，改为 `rc: Arc<RcState>` |

> 未改动：`crates/app/src/http.rs`、`crates/app/ui/*`、`crates/app/src/config.rs`、`crates/plugin/*`
> —— 这些文件在 `git status` 里显示为 `M`，是**用户在本阶段之前已有的未提交改动**，本阶段未触碰。

### 2.3 删除

| 位置 | 内容 | 理由 |
|---|---|---|
| `crates/rclone/src/proc.rs` | `RcloneManager::detect_config_path()`（24 行，含 `rclone config file` 子进程 + `%APPDATA%\rclone\rclone.conf` 兜底） | **死代码**（全仓库 0 调用）+ 用子进程拿配置路径，与 §7.2 直接冲突；能力由 `RcClient::config_paths()`（RC 调用）取代 |

---

## 3. 架构变化

### 之前（耦合）

```
app ──ctx.mgr: Arc<RcloneManager>──────────────► 进程管理
                    │  .client: RwLock<Arc<RcClient>>
                    └──────────────────────────► RC 客户端
        （想调一个 API，必须穿过进程管理器）
kv/http ──RealCaps.mgr / HttpState.mgr──► 同上
```

### 之后（解耦）

```
app ──ctx.rc: Arc<RcState>──► 连接状态 ──► RcClient（HTTP/鉴权/方法）
  │                              ▲
  └──ctx.mgr: Arc<RcloneManager>─┘  只在启动/连接后 set_connected(...)
         （进程管理 → 发布客户端，反向不依赖）
kv ──RealCaps.rc──► RcState
http ──HttpState.mgr──► 仅剩 cat_to_file（阶段 7 处理）
```

关键点：依赖方向从「双向纠缠」变成「进程管理 → 连接状态 ← UI」。UI 不再能（也不需要）通过进程管理器访问 RC。

### 新增的接缝 API

| API | 用途 | 阶段 3 的用法 |
|---|---|---|
| `RcState::client()` | 取当前客户端（无 `Option`，不改变旧调用形态） | 直接替代 |
| `RcState::status()` | `Disconnected/Connected/Stale` | 离线模式与陈旧标记 |
| `RcState::endpoint()` / `Endpoint::display()` | 当前端点（**不含密码明文**） | 连接页回显 |
| `RcState::last_error()` | 最近一次失败原因（已脱敏） | 错误提示 |
| `RcState::set_connected/set_disconnected` | 状态迁移；断开**保留**客户端与快照 | 断线重连、保留快照 |
| `RcClient::config_paths()` | `config/paths` → 真实 `rclone.conf` 路径 | 取代子进程 |
| `RcClient::rc_methods()` | `rc/list` → 该实例支持的方法集合 | 特性探测 |
| `VersionInfo::at_least(major, minor)` | 版本门槛（`decomposed`，缺失时解析字符串） | 「rclone 过旧」提示 |
| `RcErrorKind::{Connect,Auth,Unsupported,Api,Decode,Timeout}` | 错误分类 + `label()`/`hint()` | 分开报错（§6） |

---

## 4. 验证

> 按用户选择，本阶段**只做 `cargo check`**，不构建 exe、不跑测试套件。

| 命令 | 结果 |
|---|---|
| `cargo check -p rcgui-rc -p rclone_client -p rclone-gui --all-targets` | exit 0（含 bin / examples / `#[cfg(test)]`） |
| `cargo check --workspace` | exit 0（含 `imagehost-plugin`） |
| 警告 | 仅 2 条**既有**警告：`已内置 rclone.exe（82 MB）`（阶段 7 处理）、`encodeURIComponent` 非 snake_case（插件侧，与本次无关） |

### 不变量断言（grep 实测）

| 断言 | 结果 |
|---|---|
| `rcgui-rc` 内 `std::process`/`tokio::process`/`Command`/`kill(` | 0 处（唯一命中是文档注释里的说明文字） |
| app 内 `mgr.client` | 0 处 |
| `detect_config_path` / `rclone config file` 子进程 | 0 处 |
| `ctx.rc.client().await` | 36 处（与改造前 `mgr.client.read()` 数量一致，逐处等价） |

### 未做的验证（明确声明）

- 未运行程序、未做端到端 CRUD（用户选择 check-only）。
- 阶段中途用户的 `rclone-gui` 与 `rclone`（端口 5574）已退出，无法再做活体探测；
  `rc/list` 的解析形态依据本阶段开始时的实测响应（`{"calls":[{"Path":…}]}`，95 个方法）编写，
  并保留了根数组/`calls`/`Calls` 三种容错分支。

---

## 5. 行为保持性说明（为什么这是等价替换）

| 原行为 | 现行为 |
|---|---|
| 启动时 `RcloneManager::new()` 内置一个占位客户端（`127.0.0.1:5572`，无鉴权） | `RcState::new()` 内置同样的占位客户端，`status=Disconnected` |
| `start_managed` 复用现有实例 / 换端口 / 拉起 rcd 后写 `*client.write()` | 同样逻辑，只是写入 `RcState`（`set_connected`） |
| `attach` 校验后写客户端 | 同样逻辑，失败时额外记录 `last_error` |
| 退出：清挂载 → 停 serve → `core/quit` → 轮询退出 → `child.kill()` | **未改**（指南禁止主动终止 rclone，留到阶段 7） |
| RC 错误统一 `anyhow!("…")` | 仍是 `anyhow`，但底层 `RcError` 已带分类（错误文案格式保持 `method: detail`） |
| `timeout(...).and_then(|r| r)` 展开成 `anyhow::Result` | 增加 `.map_err(anyhow::Error::from)`，语义相同 |

---

## 6. 剩余耦合（阶段 7 清理清单）

| 位置 | 内容 | 处理阶段 |
|---|---|---|
| `main.rs:1274` | `start_managed(...)`（自动拉起 `rclone rcd`） | 3/7 |
| `main.rs:1300-1301` | `attach(...)`（连接配置里的端点，无界面） | 3 改造为连接页 |
| `main.rs:1479`、`1956` | `mgr.stop()`（`child.kill()`） | 7 |
| `main.rs:1949` | `is_managed_running()` + `core_quit()` | 7 |
| `main.rs:1830` | `spawn_http` 传 `mgr` 给 `HttpState` | 7 |
| `http.rs:361-362` | `cat_to_file()`（`rclone cat` 子进程，图床直链回源） | 7（改 `operations/cat` 或 HTTP 回源，需先确认实例支持） |
| `build.rs`、`embedded_rclone.rs`、`extract_embedded_rclone()` | 内嵌/释放 rclone | 7 |

以上全部**保留可编译、行为不变**，符合 §7.2「先迁移调用方，再删进程管理」。

---

## 7. 风险与假设

| 等级 | 项 | 说明 / 缓解 |
|---|---|---|
| 中 | 仅静态验证 | 未链接、未运行。风险集中在「编译通过但运行期行为差异」，本次改动以等价替换为主；阶段 3 首次运行时会做端到端握手验证 |
| 中 | `RcError` 替换了 `anyhow` 作为底层错误 | 依赖 anyhow 的 blanket `From<E: StdError>` 保证 `?` 兼容；已实测编译通过 |
| 低 | `rc/list` 解析对版本差异敏感 | 三种形态容错 + 失败即返回 `RcError`（不会 panic） |
| 低 | `RcloneManager` 现在是「发布者」，若未来多处同时发布会覆盖 | 首版只有一个活动端点（AGENTS §3 允许），阶段 4 引入多档案时再引入世代号 |
| 低 | `Cargo.lock` 被 cargo 自动更新（新增 `rcgui-rc` 条目） | 仅增量条目，无版本变化 |

**假设**：托管模式（自动拉起 rcd）在阶段 2-6 期间继续可用，因此 `start_managed` 未删除。是否保留为「高级开关」还是彻底移除，留待阶段 7 决策（阶段 1 报告问题 1）。

---

## 8. 下一阶段

**阶段 3 · 连接状态与离线 UI**，具体拆解：

1. 握手流程：`core/version` → `at_least(1,60)` 版本门槛 → `config/paths`（真实配置路径）→ `rc_methods`（特性表）；缺失接口按 `RcErrorKind::Unsupported` 明确提示。
2. 连接页：地址/端口/用户名/密码/记住凭据；默认建议 `http://127.0.0.1:5572`；**删除** `find_reusable`（`port..port+10` 凭据探测）与 `pick_free_port` 的隐式换端口。
3. 离线模式：启动不连接也能看/改本地数据；`ConnStatus::Stale` 在 UI 明确标注；提供「重试/重连」。
4. 轮询纪律：窗口不可见即暂停；同类请求单飞 + 序号防陈旧覆盖；`apply_global_limit` 从「每 2s 写 `core/bwlimit`」改为仅在值变化时写。

验收：无 rclone 进程时能启动并操作本地数据；连接后状态为 live；连接失败时四类错误文案可区分。
