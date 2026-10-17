# 阶段 5B / 6 / 7 报告：导入应用、恢复脚本、清理与文档

> 覆盖范围：`docs/refactor/phase-5a-rcgui-conf.md` 之后的全部工作，以及后续的产品调整。
> 对应代码状态：HEAD `a463f90` 之后的整套工作区改动。

---

## 阶段 5B：把导入真正应用到本地 rclone

**目标**：`rcgui.conf` 不只写进 RCGUI 自己的档案，还要在用户点「应用导入」后落到本地 rclone。

**实现**：`crates/app/src/main.rs::apply_bundle_to_rclone`

| 对象 | RC 调用 | 备注 |
|---|---|---|
| 远程 | `config/create`（不存在）/ `config/update`（已存在） | 只覆盖给出的参数键，未随包导出的凭据字段保持原值 |
| 挂载 | `mount/mount` | 已挂载的同盘符跳过，不重复挂载 |
| 服务 | `serve/start` | 返回 id 即视为成功 |

**安全边界**：
- 应用前先备份档案（`conf_io::apply` → `backup_profile`），失败可回滚；
- 全程不写 `rclone.conf` 文件、不重启 rclone 进程；
- 失败项逐条返回，成功的条目保留，汇总提示里列出失败原因。

**验收**：`cargo check --workspace --all-targets` = 0；`conf_io` 相关单元测试覆盖
预览/应用/凭据策略/旧格式兼容。

---

## 阶段 6：开机恢复脚本

**目标**：生成一个 PowerShell 脚本，只连接**已经在运行且启用 RC** 的 rclone，
按顺序恢复挂载与服务；端点不可用时明确失败退出。

**实现**：`crates/profile/src/restore.rs`

- 健康检查（`core/version`）失败 → `exit 1`，**不启动、不终止 rclone**；
- 按序 `mount/mount`、`serve/start`；
- 参数用 `ps_quote` 转义 + `ConvertTo-Json` 构造，避免字符串拼接导致的注入/错解析；
- **RC 密码与 serve 密码不写入脚本**；
- 输出到 `<数据目录>/scripts/restore.ps1` + `manifest.json`（记录生成物，清理只按 manifest 判断）；
- 设置页卡片含只读预览与「复制脚本」，生成后**不自动执行**、**不注册开机项**。

**验收**：32→33 条单元测试（转义、manifest、失败退出路径）。

**遗留**：未在真实 PowerShell 环境端到端执行过。

---

## 阶段 7：清理与文档

### 删除清单（均已确认无引用后移除）

| 对象 | 说明 |
|---|---|
| `vendor/rclone.exe`（82.8 MB） | 内嵌 rclone，RCGUI 不再拥有进程生命周期 |
| `.gitattributes`（LFS 规则） | 只为 vendor 服务 |
| `scripts/fetch-rclone.ps1` | 拉取/校验内嵌 rclone |
| `crates/app/build.rs` 的 rclone 嵌入块 | `include_bytes!` 不再需要 |
| `crates/rclone/examples/smoke.rs` | RC 冒烟测试示例 |
| `start_managed / stop / is_managed_running / cat_to_file / open_rclone_log / pick_free_port / port_in_use / quiet_command` | 进程管理入口 |
| `HttpState.mgr / exe`、`Ctx.exe`、`RcloneCfg` legacy 字段 | 托管模式残留 |

**保留**：插件 WASM 构建流程（`build.rs` 的 `IMAGEHOST_WASM` 与「先 WASM 后应用」顺序）。

### 文档

- `README.md`：改名 RCGUI，重写 rclone 说明、数据目录、项目结构；
- `进度总结.md`：重写为当前实现（旧版本描述的是托管模式，已过时）；
- `docs/refactor/phase-{1,2,3,4,5a,5b-7}.md`。

### 防回归测试

- `crate_has_no_process_spawning`：源码级断言 `rclone_client` 不含进程派生代码。

---

## 后续产品调整（预设移除与自动持久化）

### 需求
用户不再需要「挂载预设 / 服务预设」：导入一次就应该持续生效，而不是每次重新配置。

### 处理
1. **删除预设功能**：UI 上的预设卡片、编辑模式、`auto-mount`/`auto-run` 开关；
   Rust 侧的 `refresh_presets`、6 个 `Cmd::MountPreset*` / `ServePreset*` 分支、
   以及挂载/服务成功后的预设落盘。
2. **改为自动持久化 + 重放**（`crates/app/src/persist.rs`）：
   用户实际执行过的挂载/服务被记进档案，连接成功后自动重建；卸载/停止即忘记。
3. **修复导出得到 0**：预设删除后 `profile.mounts/serves` 恒空，导致导出的
   `rcgui.conf` 里挂载/服务数都是 0。改为从运行中的 rclone 状态还原
   （`mount/listmounts` 的 `mountPoints[].Fs/MountPoint`、`serve/list` 的 `params`），
   并对早期导出文件做 `normalize_legacy`（从 `serve_names` 还原服务）。

### 影响面
- `main.slint`：删除 `MountPresetRow`/`ServePresetRow`、预设卡片与回调；
- `config.rs` 的 `auto_mounts/auto_serves/serve_names` 保留为**档案的内存视图**
  （落盘唯一入口仍是 `profiles.rs`），不再是「预设」语义；
- 远程页修复：重构时误删的「类型 / 用量 / 编辑 / 删除」列已恢复。

---

## 剩余风险

1. `serve/list` 不返回密码、`listmounts` 不返回只读/缓存模式 —— 导出时只能按记录/默认值填补，已在 `warnings` 中声明；
2. 早期导出的 `rcgui.conf` 里挂载信息确实不存在，无法还原，需要重新导出；
3. 恢复脚本与「管理员运行」提权路径未做真实环境端到端验证；
4. 所有改动仍可能包含未 review 的用户原有修改（提交前应逐文件确认）。
