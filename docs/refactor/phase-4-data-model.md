# RCGUI 重构 · 阶段 4：数据模型与持久化

> 依据：`AGENTS.md` §4（配置文件、档案、导入导出）、§7.4。
> 前置：[phase-1-inventory.md](./phase-1-inventory.md) · [phase-2-decoupling.md](./phase-2-decoupling.md) · [phase-3-connection.md](./phase-3-connection.md)
> 完成时间：2026-10-17 · 基线 HEAD `a463f90`（用户未提交改动全部保留）
> 决策：数据目录采用 **`%APPDATA%\RCGUI`**（用户确认）。

---

## 1. 判定标准与本阶段结论

| 判定项 | 结果 |
|---|---|
| 是否有版本化的挂载/服务/远程档案模型 | ✅ 新 crate `rcgui-profile`：`schema_version = 1` + `ProfileDoc` |
| 持久化配置是否与运行态分离 | ✅ 档案（期望配置）在 `rcgui-profile`；运行态 DTO（`MountInfo`/`ServeRow`）仍在 `rcgui-rc`/UI，二者不互相推导 |
| 是否还有「运行态当配置用」的路径 | ✅ 没有：`mount/listmounts` 只用于展示与「是否在跑」判定 |
| 数据目录是否自有 + 旧目录迁移 + 回滚 | ✅ `%APPDATA%\RCGUI`；**复制**迁移（旧目录不动）+ `migration.json` 回滚说明；迁移失败自动回退旧目录 |
| 是否只有一个落盘写入口 | ✅ `profiles::save_from_cfg()` 是唯一写档案的函数；`config::save()` 先档案后偏好 |
| 四态（已保存/运行中/待生效/陈旧）是否可判定 | ⚠️ 模型层已提供 `desired_state()` + `normalize_mount_point()`（有单测），**尚未接入 UI**（留到阶段 5） |

---

## 2. 新增 crate `crates/profile`（包名 `rcgui-profile`，377 行）

只依赖 `serde` / `serde_json` / `chrono`——不依赖 UI、不依赖网络、不依赖进程管理，因此可以被阶段 5（导入导出）与阶段 6（恢复脚本）直接复用。

| 部分 | 内容 |
|---|---|
| 版本 | `pub const SCHEMA_VERSION: u32 = 1`；任何字段增删必须递增，并在 `Store::load` 写迁移分支 |
| 模型 | `ProfileDoc { schema_version, meta{id,name,created_at,updated_at}, connections[], active_connection, mounts[], serves[], serve_names[], remotes[] }` |
| 期望配置 | `MountProfile{remote,sub_path,point,ro,cache,enabled}`、`ServeProfile{stype,remote,sub_path,addr,user,pass,enabled}`、`ServeNameProfile` |
| 本地副本 | `RemoteNote{name,rtype,usage}`：**仅非敏感元信息**；完整参数（含 OAuth token）只在显式导出时按需附带 |
| 连接档案 | `ConnectionProfile{mode,url,port,user,has_pass}`：**明文密码永不写入档案**，只记「有没有」 |
| 存储 | `Store::load/save`：JSON + 临时文件改名（原子写）；读时校验版本 |
| 版本防线 | 档案版本 > 本程序支持 → `StoreError::VersionTooNew`，**拒绝写入**，避免新版写的数据被旧版覆盖损坏 |
| 状态判定 | `DesiredState{Saved,Running,Pending,Stale}` + `desired_state(enabled,running,pending,connected)`；`normalize_mount_point()`（`"T:"`/`"T:\\"`/`"t"` 归一，历史 bug 来源）+ `MountProfile::matches_running()` |

单测覆盖（`cargo check --all-targets` 会编译它们）：默认文档、缺字段容错、盘符归一化、状态矩阵、存读往返、拒绝未来版本、文件缺失不算错误。

---

## 3. 落盘职责划分

| 文件 | 内容 | 谁写 |
|---|---|---|
| `<数据目录>\config.json` | 应用偏好：主题/强调色/自启/内嵌 HTTP/插件启用表/限速/路径历史/`rclone{mode,exe,port,user,pass,url}` | `config::save()` |
| `<数据目录>\profiles\default.json` | **档案**：`schema_version` + meta + 连接摘要 + 挂载 + 服务 + 预设名 + 远程元信息 | `profiles::save_from_cfg()`（唯一入口） |
| `<数据目录>\migration.json` | 迁移记录：来源/目标/时间/文件数/字节数/跳过项/回滚说明 | `config::resolve_data_dir()` 一次 |
| `%APPDATA%\rclone\rclone.conf` | rclone 自己的配置 | **rclone**，RCGUI 只读（通过 `config/paths` 发现路径） |

关键改动：`AppConfig.auto_mounts / auto_serves / serve_names` 加 `#[serde(skip_serializing)]` ——
它们现在是**内存聚合视图**，不再写进 `config.json`，唯一落盘位置是档案文档。
这样既把持久化结构换成了版本化档案，又不用同时改 50 处 UI 读写点（回归风险最低）。旧版
`config.json` 里的同名字段仍能解析，会被 `profiles::load_doc()` 迁移进档案。

---

## 4. 数据目录迁移

```
%APPDATA%\rclone-gui   （旧，实测 6 个文件 / 7.5 MB：imagehost.wasm 7.3 MB + rcd.log 496 KB + config.json + cache）
        │  首次启动时「复制」
        ▼
%APPDATA%\RCGUI\
  ├─ config.json            （应用偏好）
  ├─ profiles\default.json  （档案）
  ├─ plugins\ cache\ tmp\ rcd.log …
  └─ migration.json         （回滚说明）
```

设计要点（每条都对应指南要求）：

| 要求 | 实现 |
|---|---|
| 先复制、保留回滚路径、不删旧目录 | `copy_tree()` 只读旧目录；旧目录从头到尾没有任何写操作 |
| 迁移失败不能让程序不可用 | 复制失败 → 删除本次创建的不完整副本 → **回退使用旧目录**并 toast 提示 |
| 单个文件被占用不能拖垮迁移 | 尽力而为：单文件失败（典型是运行中的 rclone 占着 `rcd.log`）只记入 `skipped`，不中断；`migration.json` 里能看到跳过了什么 |
| 不复制符号链接 | 用 `symlink_metadata` 判类型，符号链接/设备条目跳过（避免目标逃出数据目录） |
| 只做一次 | 新目录存在即直接返回；`new_dir` 在进入迁移前已确认为「不存在」，因此失败清理只会删本次自己创建的目录 |
| 用户知情 | 迁移完成/失败/跳过都会在启动后 toast（`profiles::push_notice` → `take_notices`） |

---

## 5. 写入顺序（防丢数据）

```rust
pub fn save(cfg: &AppConfig) -> Result<()> {
    profiles::save_from_cfg(cfg)?;   // 1) 档案先写：挂载/服务的唯一落盘位置
    write_atomic(&config_path(), ..)?; // 2) 偏好后写
}
```

顺序不能反：`config.json` 已不含挂载/服务字段，若先写它而档案写失败，这些预设就只剩内存一份、重启即丢。
代价是「档案不可写 ⇒ 偏好也不保存」，但这是**显式报错**而不是静默丢数据。

---

## 6. 远程本地副本

`Cmd::RemotesRefresh` 成功后调用 `profiles::set_remotes(&full)`，把 `(名字, 类型, 用量)` 写入档案的 `remotes[]`（内容未变则不写盘）。这满足「RCGUI 有自己的远程副本，离线也能看到有哪些远程」，同时**不复制凭据**。

---

## 7. 验证

> 口径不变：只做 `cargo check`，不构建 exe、不跑测试。

| 命令 | 结果 |
|---|---|
| `cargo check -p rcgui-profile -p rcgui-rc -p rclone_client -p rclone-gui --all-targets` | **exit 0** |
| `cargo check --workspace` | **exit 0** |
| 警告 | 仅 2 条既有警告（内嵌 rclone 82 MB、插件命名） |
| 确认新 crate 参与编译 | 输出含 `Checking rcgui-profile v0.1.0` |

不变量断言：

| 断言 | 结果 |
|---|---|
| `config.json` 不再序列化三个列表 | `skip_serializing` 3 处 |
| 数据目录指向 `%APPDATA%\RCGUI` | 1 处（`join("RCGUI")`） |
| 迁移标记与回滚说明 | `migration.json` 2 处引用 |
| 档案写入口唯一 | `save_from_cfg` 仅 3 处引用（定义 + `config::save` + 测试） |
| 现有 `config::save` 调用点 | 20 处全部自动走新链路（无需逐个改） |

**未运行程序**，因此 `%APPDATA%\RCGUI` 目前不存在（已确认），真正的迁移会在用户首次运行新构建时发生。

---

## 8. 偏差与风险

| 等级 | 项 | 说明 |
|---|---|---|
| 中 | UI 仍通过 `AppConfig` 读写预设 | 阶段 4 用「内存聚合 + `skip_serializing`」过渡，避免一次改 50 处读写点。档案文档已是唯一落盘位置，不存在两份数据；但 `ProfileStore` 作为 UI 直接数据源要到阶段 5 才接 |
| 中 | `desired_state()` 未接 UI | 模型与单测已就绪，UI 徽标（已保存/运行中/待生效/陈旧）留到阶段 5 与导入预览一起做 |
| 中 | 迁移是「复制」而非「移动」 | 旧目录会长期占 ~7.5 MB；这是刻意的回滚保障，阶段 7 文档里给出清理说明 |
| 低 | 首次启动多一次 7.5 MB 复制 | 一次性，失败自动回退，不影响启动 |
| 低 | `connections[]` 目前只写一条（来自 `cfg.rclone`） | 多连接/多档案属阶段 5 的 `rcgui.conf` 范围；schema 已预留字段，不需要再升版本 |

---

## 9. 下一阶段

**阶段 5 · 导入/导出（`rcgui.conf`）**：

1. 定义 `rcgui.conf`：JSON 单文件（`schema_version` + profile 元信息 + 远程配置 + 期望挂载/服务 + 可选脚本清单），复用 `rcgui_profile::ProfileDoc` 作为内核。
2. 导入流程：解析 → 校验 → **预览**（档案名、远程列表、挂载/服务数量、目标 rclone 配置路径、合并差异）→ 应用（先备份 `rclone.conf`）→ 失败可回滚。
3. 敏感数据：导出前二次确认 + 明示含 OAuth token/密码；`obscure` 不是加密要讲清楚；默认不写日志。
4. 不静默覆盖正在被 rclone 使用的配置：优先走 `config/create|update`，全文件替换必须先备份并提示「需手动重启 rclone」。
5. 四态徽标接入 UI（`desired_state`）。

验收：导入预览能列出差异；覆盖前有备份；失败可回滚；导出有敏感数据提示。
