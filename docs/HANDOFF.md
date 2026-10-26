# RCGUI 交接总结（供新对话使用）

> 生成时间：2026-10-17 · 仓库：`D:\work\Documents\projects\rclone_gui_windows`
> 分支 `main` · HEAD `c6e328a` · 工作树**干净** · 与 `origin/main` 完全同步（0 ahead / 0 behind）

---

## 一、当前状态速览

| 项目 | 值 |
|---|---|
| `cargo test --workspace` | **10 个测试二进制全绿，共 111 个测试** |
| `cargo check --workspace --all-targets` | 0 错误 |
| release 产物 | `target\release\rclone-gui.exe`，**40,597,504 字节（38.7 MB）**，18:00:57 |
| SHA256 | `814935C66C3C57B8605EF4E4CE022FEEA61C18953C2F9FAE2C3A72232A2E4590` |
| 构建方式 | 先 WASM 后应用 |

```powershell
cargo build -p imagehost-plugin --target wasm32-wasip1 --release
cargo build -p rclone-gui --release
```

### 已推送的提交

```
c6e328a fix(transfers): 所有后台任务都被错误标上同一个 serve 标签
9afe6b5 fix(mount): 挂载后立即卸载会误报「盘符未出现」
85371d2 fix(mount): 挂载成功后不提示、spinner 不消失
f3873b9 refactor: 改造为 RCGUI（只连外部 RC，不托管 rclone 进程）
a463f90 fix(app): 彻底移除 job_list/job_status 轮询…（重构前基线）
```

远端：https://github.com/h897105167/rclone_gui_windows

---

## 二、待完成的任务：按 remote 分配代理配置

**这是本次新对话要做的唯一任务，代码尚未开工**（已确认全仓库 `proxy` 关键字命中数 = 0）。

### 已完成的先期验证（可直接复用，无需重复）

目标 rclone 版本：**v1.75.4**，RC 命令总数 97。

**1. `config/unset` 存在且可用** —— 这是移除 remote 级代理的官方途径：

| 命令 | 参数签名 |
|---|---|
| `config/unset` | `name`（remote 名）、`keys`（要删除的键名列表）；返回 `removed`（实际删除的键） |
| `config/update` | `name`、`parameters`（`{key: value}` 映射）、`opt`（`obscure` / `noObscure` / `noInteractive` / `continue` / `state` / `result`） |
| `config/get` | `name`；返回该 remote 的全部键值（读现有值用这个，不要用 `config/dump`，后者含全部凭据） |

**2. 当前 teldrive remote 的现状（只读查看，未做任何修改）**

`config/get {name:"teldrive"}` 返回的键：`api_host`、`api_key`、`channel_id`、`chunk_size`、`encoding`、`encrypt_files`、`hash_enabled`、`page_size`、`type`、`upload_concurrency`。

**其中没有任何 `proxy` 相关键** —— 意味着当前是「用户未手动设置代理」的干净基线，测试时可据此判断「原有值」为空。⚠️ **注意：这个 remote 的 `api_key` 是真实凭据，绝对不要写进测试、日志或提交。**

**3. 尚未验证的关键项（新对话必须先做）**

- `override.http_proxy` 是否真能通过 `config/update` 写入并对 HTTP 类 remote 生效（**尚未实测**）
- SOCKS5 / HTTPS 代理的 rclone 参数名与行为（**尚未实测**）
- FTP / SFTP 等类型的独立代理选项（**尚未实测**，很可能不存在，需要明确报「不支持」）
- `config/unset` 移除后 rclone.conf 里的实际残留
- 代理用户名密码经 `config/update` 写入后，在 **rclone.conf 中的存储形式**（明文？obscure？）—— 决定 UI 提示文案

**要求：以上验证必须在隔离的临时 rclone 配置 + 独立 rclone 实例上做，绝不碰用户的真实配置与云端数据。**

隔离环境建议：
```powershell
$iso = Join-Path $env:TEMP 'rcgui-proxy-test'   # 注意：本机 %TEMP% 有时不可写，备选 target\proxy-test
rclone --config "$iso\rclone.conf" rcd --rc-addr 127.0.0.1:5599 --rc-user t --rc-pass t
```

---

## 三、任务需求要点（用户原始要求，勿遗漏）

### 定位
**remote 级分流** —— 不是按文件、域名或单次请求分流。

### 数据模型
- 代理配置：稳定 ID、名称、协议、地址、端口、可选用户名/密码
- 另存 remote → 代理配置 ID 的映射
- 协议至少支持**经验证的** HTTP / HTTPS / SOCKS5
- 地址支持主机名或 IP；端口限制 1–65535
- 用 URL 解析/构造库处理地址，正确编码用户名密码（**覆盖特殊字符与 IPv6**）

### 界面（「远程」页）
- 代理配置的新增 / 编辑 / 删除
- 删除仍被 remote 使用的配置时，**提示用户处理关联**
- remote 列表增加「代理」列 + 下拉框：`不代理` / 已保存的配置
- 「不代理」= **不设置 RCGUI 的 remote 级代理**；要提示 rclone 仍可能继承 `HTTP_PROXY`/`HTTPS_PROXY`。
  **不要描述成「强制直连」。**
- 「不支持或未验证的 remote 要明确显示原因，不要假装代理已生效」

### 行为
- 代理配置与绑定**先存 RCGUI 档案**；RC 未连接 → 标记「待应用」；连上后再走 RC 更新
- 切换代理只更新对应 remote，**不重启/不停止 rclone**
- 代理只在 remote 创建时生效；若该 remote 当前有挂载或服务，**提示需要重新挂载/启动**，
  首版**不要自动**卸载或重建（除非用户明确确认）
- **不能静默覆盖用户原有代理**；切回「不代理」也**不能误删原有设置**
  → 保存并恢复原值，或在无法安全恢复时提示冲突

### 凭据与导入导出
- 密码不写日志，UI 默认遮蔽
- 代理凭据纳入现有「默认排除凭据」的导出策略；导入后标记需要补充凭据
- 未确认 rclone.conf 存储形式受保护前，**按明文敏感信息提示**；`obscure` 不能称为加密

### 测试（只许用隔离配置与本机代理）
代理档案与绑定持久化、URL 特殊字符编码、设置/移除 remote 代理、**保留用户原有代理**、
凭据导出策略、**活动挂载时的提示状态**。

### 交付
报告：支持范围、验证命令及**退出码**、未验证项。

---

## 四、关键文件地图

| 文件 | 行数 | 职责 |
|---|---|---|
| `crates/app/src/main.rs` | 5413 | Slint UI 绑定、命令循环 `handle_cmd`、挂载/服务/远程逻辑、`apply_bundle_to_rclone` |
| `crates/app/src/persist.rs` | 296 | **用户操作的持久化 + 连接后重放**（挂载/服务的记住与恢复） |
| `crates/app/src/conf_io.rs` | 473 | `rcgui.conf` 构建/预览/应用；`live_serves_from`/`live_mounts_from` |
| `crates/app/src/profiles.rs` | 333 | 档案文档 ⇄ AppConfig 适配层（挂载/服务/连接的唯一落盘入口） |
| `crates/app/src/config.rs` | — | `AppConfig`、数据目录解析、原子写、旧字段迁移 |
| `crates/app/ui/main.slint` | 2928 | 全部 UI |
| `crates/profile/src/bundle.rs` | 641 | `RcguiConf` 模型、`apply_secret_policy`（凭据策略唯一实现）、`normalize_legacy` |
| `crates/profile/src/lib.rs` | 426 | `ProfileDoc`、`Store`、`MountProfile`/`ServeProfile`/`RemoteNote` |
| `crates/profile/src/restore.rs` | — | 恢复脚本生成 |
| `crates/rc/src/client.rs` | 481 | `RcClient`：`config_create/update/delete/dump/providers`、`mount*`、`serve*` |
| `crates/rc/src/conn.rs` | 194 | `RcState`（Disconnected/Connected/Stale） |
| `crates/rc/src/handshake.rs` | 212 | 三步握手 + 18 项能力表（**加代理可能要考虑能力门控**） |

### 要做代理功能，建议改动点

1. `crates/rc/src/client.rs`：加 `config_unset(name, keys)`、`config_get(name)` 封装
2. `crates/profile/src/lib.rs`：`ProfileDoc` 增加 `proxies: Vec<ProxyProfile>` + `remote_proxy: Vec<RemoteProxyBinding>`（或 HashMap）
3. `crates/app/src/profiles.rs`：新列表的读写互转 + `save_from_cfg`
4. `crates/app/src/persist.rs`：参照 `remember_mount`/`forget_mount` 的写法，加代理绑定的「待应用 → 连接后应用」逻辑
5. `crates/app/ui/main.slint`：远程页加「代理」列 + 下拉框 + 代理配置管理卡片
6. `crates/app/src/main.rs`：新的若干 `Cmd::Proxy*` 分支 + UI 回调绑定
7. `crates/profile/src/bundle.rs`：`apply_secret_policy` 要把代理密码纳入剥离策略

---

## 五、架构与产品边界（重要，勿违反）

- **RCGUI 不拥有 rclone 进程生命周期**：不启动、不停止、不重启 rclone；不内嵌 rclone
- 只连接用户指定的 RC endpoint；连不上 → 离线模式，**绝不回退成启动 rclone**
- 关闭 GUI 只停轮询/释放连接，**不调 `core/quit`**、不杀进程、不卸载挂载、不停 serve
- 挂载/服务只由用户显式操作（或导入后点「应用」）触发
- `AGENTS.md` 有完整的 7 阶段重构指南，改动前应读

### 已有的自动持久化机制（代理功能可复用其模式）

`crates/app/src/persist.rs`：用户实际执行过的挂载/服务会记进档案，
连接成功后自动重放（已存在的跳过）；卸载/停止即忘记。
代理的「待应用」逻辑应该沿用这个思路，但**不要**自动重建挂载（用户明确要求首版不自动）。

---

## 六、环境与踩坑（必读）

1. **切勿用 `Get-Content` + `Set-Content -Encoding utf8` 改写源码**。PowerShell 5.1 会按 GBK 读入再按 UTF-8 写出，逐字重编码会破坏**所有中文注释**。本项目曾因此损坏 `main.rs`（约 600 处字符），靠会话转录重放 114 次 edit 才恢复。改写源码请用 `edit`/`write` 工具，或 Python `open(..., encoding='utf-8', newline='')`。

2. **`%TEMP%` 在本机沙箱有时不可写**（os error 5）。单元测试的临时目录放在测试可执行文件旁（已有 `test_dir()` 惯例可抄）。

3. **构建报 `os error 5 / 拒绝访问`** 通常是运行中的 exe 锁住了文件。若遇到杀不掉的进程，多半是**以管理员权限启动的实例**（Medium 完整性级别的 shell 无法结束 High 进程）——需用户手动在任务管理器关闭。

4. **UI 自动化限制**：Slint 内容区的鼠标点击在本机**无法可靠送达**（导航项点了不切换）。可用：
   - `SetProcessDPIAware()` 后 `GetClientRect` + `ClientToScreen` 获取真实坐标（窗口 2100×1330）
   - `PrintWindow` 截图（覆盖遮挡时用 `CopyFromScreen`）
   - 但点击内容区基本无效 —— 验证优先用**单元测试 + 真实 RC 状态检查**，而非 UI 点击

5. **PowerShell 变量名大小写不敏感**（`$M` 与 `$m` 同一个），已两次因它写坏文件。

6. **`Invoke-RestMethod` 调用 RC** 的标准写法：
```powershell
$cfg = Get-Content "$env:APPDATA\rclone-gui\config.json" -Raw | ConvertFrom-Json
$hdr = @{ Authorization = "Basic " + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(("{0}:{1}" -f $cfg.rclone.user, $cfg.rclone.pass))) }
Invoke-RestMethod -Uri "http://127.0.0.1:5574/config/get" -Method Post -Headers $hdr -ContentType "application/json" -Body '{"name":"teldrive"}'
```

7. **本机现状**：rclone v1.75.4，`rclone rcd --rc-addr 127.0.0.1:5574 --rc-user gui`；`teldrive:` 挂载在 `T:`；有 2 个 webdav serve（8080/8081，都是 `teldrive:` 根目录）。数据目录 `%APPDATA%\RCGUI`（旧 `%APPDATA%\rclone-gui` 仍保留）。

8. **git 提交注意**：用 `Out-File -Encoding utf8` 写 commit message 会带 BOM，导致提交信息首字符变成 `﻿`。正确做法：
```powershell
$body = (Get-Content "msg.txt" -Raw).TrimStart([char]0xFEFF)
[System.IO.File]::WriteAllText("msg2.txt", $body, (New-Object System.Text.UTF8Encoding($false)))
git commit -F "msg2.txt"
```

---

## 七、近期修复的三个 bug（避免回归）

| 提交 | 问题 | 根因 |
|---|---|---|
| `85371d2` | 挂载成功不提示、spinner 不消失 | `wait_mount_ready_rc` 用 `waited += step / waited -= step` 控预算，每轮被延长则净增长恒为 0 → 永不返回；且 pending 摘除后没重绘；刷新被单飞丢弃 |
| `9afe6b5` | 挂载后立即卸载误报「盘符未出现」 | 上一版修复新增的「rclone 不认了就 return false」把「用户主动卸载」误判为「挂载失败」；根目录 serve 同理 |
| `c6e328a` | 所有后台任务都被标上同一个 serve 标签 | 用 `fs.starts_with(serve.fs)` 匹配，而根目录 serve 的 fs 与任务 srcFs 都是 `teldrive:`，且 `break` 取第一个 |

**教训**：凡是「等待/轮询」逻辑必须有**基于绝对时间**的上限；凡是「取消」与「失败」必须可区分。

---

## 八、已知限制 / 未完成项

1. exe 未签名，分发有 SmartScreen 提示
2. crate/exe 名仍是 `rclone-gui`；互斥量 `rclone-gui-singleton`、注册表启动项 `RcloneGUI` 未改名
3. 「以管理员身份运行」的 UAC 提权路径未端到端实测（只验证了弹层与取消）
4. 插件包的 `requires_admin` 只是声明字段，未实现提权
5. 恢复脚本（PowerShell）只有单元测试，未在真实 PowerShell 执行
6. 图床直链 `/i/{rootId}/{path}` 端到端（缓存命中/并发/回源）未自动化
7. 后台任务的 serve 标签：子路径 serve 可准确匹配，**根目录 serve 无法归因**（rclone 不返回该信息）

---

## 九、建议的第一步

```
1. 读 AGENTS.md + crates/app/src/persist.rs（持久化模式）+ crates/rc/src/client.rs（RC 封装）
2. 搭隔离环境：独立 rclone.conf + 127.0.0.1:5599 的 rcd
3. 在隔离实例上验证 override.http_proxy 的写入/生效/移除，并查看 rclone.conf 落盘形式
4. 验证结果决定「支持范围」矩阵，再动手写代码
```
