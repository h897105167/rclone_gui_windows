# RCGUI 重构 · 阶段 5A：`rcgui.conf` 格式、导出与备份

> 依据：`AGENTS.md` §4（导入导出、敏感数据、备份回滚）、§7.5。
> 推进方式：按用户选择**分两步**——本文件是第一步（格式 + 导出 + 备份 + 导入校验），
> 第二步（预览面板 UI + 写入 rclone 配置）留待 5B。
> 前置：[phase-4-data-model.md](./phase-4-data-model.md) · 基线 HEAD `a463f90`

---

## 1. 判定标准（5A 范围内）

| 判定项 | 结果 |
|---|---|
| 是否有版本化单文件格式，文件名 `rcgui.conf` | ✅ `KIND = "rcgui.conf"`，`BUNDLE_SCHEMA_VERSION = 1` |
| 是否含 schema 版本 / 档案元信息 / 远程配置 / 期望挂载服务 / 可选脚本清单 | ✅ 全部字段齐备（`profile` + `remotes` + `script_manifest`） |
| 导入是否先解析校验、再展示摘要与差异 | ✅ `preview_file()` 只读不写，输出 `Summary` + `Diff` |
| 失败是否可恢复 | ✅ 应用导入前自动备份档案；应用失败不改动任何数据 |
| 敏感数据是否明确提示 | ✅ 文件内 `sensitive` 标记 + `warnings` + 导出 toast 告警 + `obscure ≠ 加密` 说明 |
| 是否可能静默覆盖正在使用的 rclone 配置 | ✅ **5A 完全不写 `rclone.conf`**（写 rclone 配置是 5B，需配合预览） |
| 是否防路径穿越/超大文件 | ✅ 脚本清单拒绝绝对路径与 `..`；条目数上限（远程 ≤200、挂载/服务各 ≤500） |

---

## 2. 格式设计

选择 **JSON 单文件**而不是 zip 容器，理由写在 `bundle.rs` 顶部：容器要额外防路径穿越、
重名覆盖、超大展开、校验失败；本格式只需要一个文档，脚本清单以「路径 + sha256 + 用途」
的**元数据**形式内嵌，脚本文本不随包携带——从根上避免「导入即获得可执行文件」。

```jsonc
{
  "kind": "rcgui.conf",            // 精确匹配，防止把任意 JSON 当配置包
  "schema_version": 1,             // 包格式版本（与档案 schema 独立）
  "exported_at": "2026-10-17T…",
  "app_name": "RCGUI", "app_version": "0.1.0",
  "sensitive": true,               // 是否含凭据（保守判断：有任一远程带参数即为 true）
  "profile": {                     // 直接复用 rcgui_profile::ProfileDoc
    "schema_version": 1,
    "meta": { "id": "default", "name": "默认档案", … },
    "mounts": [ { "remote": "teldrive", "point": "T:", "cache": "writes", "ro": false, "enabled": true } ],
    "serves": [ { "stype": "webdav", "remote": "teldrive", "addr": "127.0.0.1:8080", … } ],
    "serve_names": [], "remotes": [ { "name": "teldrive", "rtype": "teldrive", "usage": "" } ],
    "connections": [...], "active_connection": "default"
  },
  "remotes": [ { "name": "teldrive", "type": "teldrive", "params": { /* 可能含 token */ } } ],
  "script_manifest": [],           // 只记录，不携带脚本；导入不执行
  "warnings": [ "该文件包含远程凭据… obscure 只是可逆混淆、并非加密…" ]
}
```

校验规则（`RcguiConf::validate`，纯函数、无 IO）：`kind` 精确匹配、包版本不高于本程序、
远程名非空/不含冒号空格/大小写不重复、必须有后端类型、数量上限、脚本路径不得绝对或含 `..`。

---

## 3. 两步确认的导入流程（本阶段落地的一半）

```
[导入预览] 选文件 → read → JSON 解析 → validate → summary + diff → 暂存到 Ctx.pending_import
                    ↓ 只读，不写盘
[应用导入] 取暂存 → 备份 profiles\default.json → replace_from_bundle() 写档案 → 同步内存 cfg
                    ↓ 失败则把暂存放回，允许重试；数据不动
```

- 「应用导入」按钮在预览成功前是禁用的（`SettingsG.conf-pending`），所以**不存在绕过预览直接写盘**的路径。
- 预览只输出计数与差异（`远程 +n / -n / 保留 n；挂载 a→b，服务 c→d`），**不输出任何 `params` 内容**。

---

## 4. 改动清单

| 文件 | 行数 | 内容 |
|---|---|---|
| `crates/profile/src/bundle.rs` | 410 | **新增**：`RcguiConf` / `RemoteConf` / `ScriptEntry` / `Summary` / `Diff` / `BundleError`，`validate()`、`summary()`、`diff()`，9 个单测 |
| `crates/profile/src/lib.rs` | 384 | 导出 `bundle` 模块与 `BUNDLE_KIND/BUNDLE_SCHEMA_VERSION` |
| `crates/app/src/conf_io.rs` | 241 | **新增**：`build_bundle()`（在线带参数/离线仅档案）、`write_bundle()`（原子写）、`preview_file()`、`backup_profile()`、`apply()`、`describe()`，3 个单测 |
| `crates/app/src/profiles.rs` | 341 | 新增 `current_doc()`、`replace_from_bundle()`（唯一写入口不变） |
| `crates/app/src/main.rs` | 5515 | `mod conf_io`、`Ctx.pending_import`、`Cmd::{ConfExport,ConfPreview,ConfApply}` 及实现、三个 rfd 文件对话框接线 |
| `crates/app/ui/main.slint` | 3001 | 新增「档案导入 / 导出（rcgui.conf）」卡片（三个按钮 + 状态文本）、`conf-pending`/`conf-info` 属性、3 个回调 |

---

## 5. 验证

| 命令 | 结果 |
|---|---|
| `cargo check -p rcgui-profile -p rcgui-rc -p rclone_client -p rclone-gui --all-targets` | **exit 0** |
| `cargo check --workspace` | **exit 0** |
| 警告 | 仅 2 条既有警告；另修掉一条自己引入的 `field source is never read`（把文件名用进预览描述） |

`.slint` 改动（新卡片、3 个回调、2 个属性）由 `build.rs → slint_build::compile` 实际编译验证。

单测（会被 `--all-targets` 编译，未运行）覆盖：自身产物可校验、拒绝外来 JSON、拒绝未来版本、
拒绝非法远程名/大小写重复、拒绝脚本路径逃逸、拒绝超量条目、摘要不含 params 内容、
差异计数、JSON 往返、离线导出不带参数、预览往返、拒绝非包 JSON。

---

## 6. 已知边界与风险

| 等级 | 项 | 说明 |
|---|---|---|
| 高 | 导出默认可含凭据 | 导出按钮一次点击即把 token 写入文件，只靠 toast 提示。5B 应加「导出前确认」对话框，并提供「不含凭据导出」选项 |
| 中 | 预览信息目前只有 toast + 设置页一行文本 | 完整预览面板（远程清单、挂载/服务表、冲突项）在 5B |
| 中 | 导入不写 rclone 配置 | 5A 只恢复 RCGUI 自己的档案；远程要真正可用仍需 rclone 侧配置。这是刻意拆分：写 rclone 配置风险高，必须与预览+备份+「需手动重连」提示一起做 |
| 低 | 不含凭据的 `rcgui.conf` 导入后远程无参数 | 文件里已标 `sensitive:false` 且 warnings 说明，不会静默 |

---

## 7. 下一步（5B）

1. 预览面板：远程清单（新增/覆盖/删除）、挂载与服务表、目标 rclone 配置路径、冲突提示。
2. 导出确认对话框 + 「不含凭据」导出选项。
3. 写入 rclone：优先 `config/create|update`；必须整文件替换时先备份 `rclone.conf`，并明确提示「需手动重启/重连 rclone」。
4. 四态徽标（`desired_state`）接入挂载/服务列表。
