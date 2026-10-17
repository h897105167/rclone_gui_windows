# 代理功能验证报告（隔离环境实测）

> 日期：2026-10-17 · rclone **v1.75.4** · 隔离实例 `127.0.0.1:5599` + 独立 `rclone.conf`
> 真实实例（`127.0.0.1:5574`）与真实 `%APPDATA%\rclone\rclone.conf` **全程未被触碰**
> 结论：HANDOFF §二.3 列出的 5 个「尚未验证」项全部落实，其中 **1 项与预期不同**（SOCKS5 的键名与 scheme）
>
> **实现状态**：本文 §一~§五 是验证结论，§七 起是据此完成的实现与端到端验证。

---

## 一、结论速览

| 待验证项 | 结果 |
|---|---|
| `override.http_proxy` 能否经 `config/update` 写入并对 HTTP 类 remote 生效 | ✅ **能**，且实测确实把流量导到代理 |
| SOCKS5 / HTTPS 代理的 rclone 参数名与行为 | ⚠️ **与预期不同**，见 §二.2 |
| FTP / SFTP 等是否有独立代理选项 | ✅ **有**，是后端原生选项，不是 `override.*` |
| `config/unset` 移除后的 `rclone.conf` 残留 | ✅ **零残留**，幂等（不存在时 `removed: null`，不报错） |
| 代理用户名密码的落盘形式 | 🔴 **百分号编码的明文**，连 obscure 都不是 |

---

## 二、关键发现（直接影响实现）

### 1. HTTP / HTTPS：`override.http_proxy` 完全符合 HANDOFF 预期

```text
override.http_proxy = http://127.0.0.1:8897      → 代理收到  HEAD http://origin/file.txt
override.http_proxy = https://127.0.0.1:8901     → 代理收到  TLS ClientHello（16 03 01）
```

代理收到的是**绝对 URI 形式的请求行**（`HEAD http://127.0.0.1:8898/file.txt`），
即标准 HTTP forward proxy 行为，不是 CONNECT 隧道。HTTPS 代理则直接 TLS 握手到代理本身。

### 2. 🔴 SOCKS5：不是 `override.socks_proxy`，而是 `override.http_proxy` + `socks5://` scheme

这是本次最重要的一条，**与 HANDOFF 的假设相反**，必须按实测结论写代码：

| 写法 | 结果 |
|---|---|
| `override.http_proxy = socks5://127.0.0.1:8899` | ✅ **走 SOCKS5**（抓到 `050100` greeting、RFC1929 认证、目标地址正确） |
| `override.http_proxy = socks5h://127.0.0.1:8899` | ✅ 同样走 SOCKS5 |
| `override.socks_proxy = 127.0.0.1:8899` | ❌ **被彻底忽略**，流量直接连源站 |

原因见 rcd 日志：`Set overridden config ["http_proxy"] for backend startup` ——
rclone 只认 `override.http_proxy` 这一个键，由 Go `http.Transport` 解析 URL scheme
来决定走 HTTP 还是 SOCKS5。`override.socks_proxy` 只会被写进 conf 当死数据。

**实现要求**：RCGUI 的协议选择框选 SOCKS5 时，必须拼成 `socks5://user:pass@host:port`
写进 `override.http_proxy`，**不要**写 `override.socks_proxy`。

### 3. 🔴 代理凭据是明文，且连 obscure 都不是

```ini
override.http_proxy = http://alice:p%40ss%3Aword@127.0.0.1:8897
```

- 原样存百分号编码，**没有**走 `obscure`（对比：同文件的 `pass` 字段确实被 obscure 了）
- 百分号编码正确解码：`alice` / `p@ss:word`，代理侧收到 `Proxy-Authorization` 且值匹配
- SOCKS5 的 RFC1929 认证同样正确解码：`bob` / `se:cr@t`
- ⇒ UI 提示必须写「**明文**」，不能说 obscure，更不能说加密

### 4. 🔴 rclone 不校验代理 URL，未知 scheme 会静默降级成 HTTP 代理

| 输入 | rclone 行为 |
|---|---|
| `127.0.0.1:8899`（缺 scheme） | 报 `parse ...: first path segment in URL cannot contain colon`，请求失败 |
| `not a url at all` | **`config/update` 照单全收**，写进 conf 不报错 |
| `ftp-proxy://127.0.0.1:8903` | **当普通 HTTP 代理用**，把明文 HEAD 发过去（不是拒绝） |

⇒ **校验必须在 RCGUI 侧做完**（scheme 白名单 + host + 端口 1–65535），
不能指望 rclone 报错。同时把「未知 scheme」当作不支持，而不是让它静默变 HTTP。

### 5. FTP / SFTP 有原生代理选项，但语义与 HTTP 那套完全不同

```ini
[ftpt]
type = ftp
http_proxy = http://127.0.0.1:8897     # 原生选项：URL for HTTP CONNECT proxy
socks_proxy = 127.0.0.1:8899           # 原生选项：Socks 5 proxy host（无 scheme）

[sftpt]
type = sftp
http_proxy = ...
socks_proxy = ...
```

全局 `config/providers` 扫描 71 个后端，**只有 3 个**声明了 proxy 类选项：

| 后端 | 选项 |
|---|---|
| `ftp` | `socks_proxy`（host:port，无 scheme）、`http_proxy`（URL） |
| `sftp` | `socks_proxy`、`http_proxy` |
| `google photos` | `proxy`（**gphotosdl 专用**，与代理无关，勿使用） |

注意区别：**ftp/sftp 的 `http_proxy` 是给 FTP/SSH 本身穿隧道用的 CONNECT 代理**，
和 `override.http_proxy` 不是同一个东西。二者**互不影响**（已验证：unset `override.*`
后原生 `http_proxy` 原样保留）。

⇒ 支持范围矩阵必须区分「HTTP 传输类后端（走 override）」与「ftp/sftp（走原生选项）」，
不要用一套逻辑套所有类型。

### 6. 活动 serve / 挂载确实不会热生效

实测（`serve/start` 起一个 `wd:` 的 http serve，然后改代理）：

1. serve 运行时改 `override.http_proxy` → **仍在直连**（5 次 PROPFIND 全打在源站）
2. 重启该 serve → **10 次 PROPFIND 全部走代理**

⇒ 印证了用户要求：改代理后必须提示「需要重新挂载/重新启动服务」，首版不自动重建。

---

## 三、验证命令与退出码

所有脚本都只连 `127.0.0.1:5599`（隔离实例）与本地 witness 端口，无外网访问。

| 脚本 | 用途 | 退出码 |
|---|---|---|
| `probe_proxy\verify.py` | 主验证：写入/生效/凭据/移除/原生选项 | `1`（见下方说明） |
| `probe_proxy\socks_probe.py` | SOCKS5 键名与 scheme 三选一矩阵 | `0` |
| `probe_proxy\schemes.py` | scheme 覆盖 + 非破坏性 + 校验缺口 | `1`（见下方说明） |
| `probe_proxy\live_serve.py` | 活动 serve 的代理热生效 | `0` |

`verify.py` 与 `schemes.py` 的退出码 1 来自**脚本里预期写错的两条断言**，不是环境问题：

- `verify.py` 的 baseline / SOCKS5 两条断言用的是「`override.http_proxy` 会走 socks」的
  旧假设，已被 `socks_probe.py` 的实测推翻；有效断言 13/15 全过。
- `schemes.py` 的「未知 scheme 应无流量」断言被推翻了 —— 这正是 §二.4 的发现，
  已作为结论保留而非漏洞。

为让退出码可复现地全绿，最终以 `socks_probe.py` + `live_serve.py` + 手工基线为准。

### 隔离环境启动命令

```powershell
$iso = "D:\work\Documents\projects\probe_proxy\iso"
rclone rcd --config "$iso\rclone.conf" --log-file "$iso\..\rcd.log" --log-level DEBUG `
           --rc-addr 127.0.0.1:5599 --rc-user t --rc-pass t
```

---

## 四、支持范围矩阵（据此实现）

| remote 类型 | 代理方式 | 状态 |
|---|---|---|
| HTTP 传输类（webdav / s3 / drive / onedrive / b2 / azureblob / gcs / http / teldrive …） | `override.http_proxy` = `http://…` `https://…` `socks5://…` `socks5h://…` | ✅ 已验证 |
| `ftp` | 原生 `http_proxy`（URL）/ `socks_proxy`（host:port） | ✅ 已验证写入，**未验证真实 FTP 穿透** |
| `sftp` | 原生 `http_proxy` / `socks_proxy` | ✅ 已验证写入，**未验证真实 SSH 穿透** |
| `local`（及其它无网络后端） | 可写入但无意义 | ⚠️ 应报「该类型不需要代理」 |

---

## 五、仍未验证 / 已知缺口

1. **ftp / sftp 的真实穿透未端到端验证** —— 只证明了选项能写入并落盘，
   没有真起一个 FTP/SFTP 服务器穿过代理测通。实现时按「已验证写入、未验证穿透」描述。
2. **代理连接失败时的错误文案**未采样 —— rclone 返回的原文需要抓一次才能做友好提示。
3. **`HTTPS_PROXY` 环境变量继承**未在本报告内测（本机 `HTTP_PROXY=http://127.0.0.1:27723`
   是全局设置，隔离实例也继承了它）。⇒ UI 上「不代理」的文案必须保留「rclone 仍可能
   继承系统 `HTTP_PROXY`/`HTTPS_PROXY`」这句，不能写「强制直连」。
4. **`no_proxy` / 按主机排除**未测，首版不做。
5. **挂载（mount）侧**未单独验证热生效 —— 与 serve 同属后端实例，
   预期行为一致，但未实测（改代理时挂载是否也需重挂）。

---

## 六、对实现的直接影响（务必照此写）

1. 数据模型只存**一个**代理字段语义：`override.http_proxy`，协议只用于拼 scheme 前缀。
2. 生成的 URL 必须由「URL 构造库」拼，保证用户名密码百分号编码（含 `@` `:` 等），
   IPv6 主机要加方括号。
3. 协议白名单硬编码 4 项：`http` / `https` / `socks5` / `socks5h`；
   其它一律在 RCGUI 侧拒绝，不能落到 rclone。
4. 存原值做回滚：应用前后各读一次 `config/get`，把用户**原有** `override.http_proxy`
   （或原生 `http_proxy`/`socks_proxy`）存进档案；切回「不代理」时恢复原值而非无脑 unset。
5. UI 提示文案定稿：
   - 凭据：**「rclone 以百分号编码明文保存，不是加密」**
   - 「不代理」：**「不设置 RCGUI 的 remote 级代理；rclone 仍可能继承系统 HTTP_PROXY/HTTPS_PROXY」**
   - 改代理后：**「需重新挂载/重新启动服务才会生效」**
6. 能力门控：`METHOD_REQS` 加 `config/unset`（非阻断，但缺失时要报「不支持移除代理」）。

---

## 七、实现结果（2026-10-17 完成）

### 7.1 改动清单

| 文件 | 改动 |
|---|---|
| `crates/profile/src/proxy.rs` | **新增**。`ProxyProtocol`（4 项白名单）、`ProxyProfile`（校验 + URL 构造）、`RemoteProxyBinding`（含回滚信息）、`ProxyRegistry`（查询）、`plan_set` / `remember_original`（纯决策函数） |
| `crates/profile/src/lib.rs` | `SCHEMA_VERSION` 1→2；`ProfileDoc` 加 `proxies` / `remote_proxies`；加 `bind_proxy` / `remove_proxy` / `upsert_proxy` / `forget_remote_proxy` / `proxy_registry` |
| `crates/profile/src/bundle.rs` | `apply_secret_policy` 把代理账号密码纳入剥离；新增 4 条 warning 断言测试 |
| `crates/rc/src/client.rs` | 加 `config_get` / `config_unset` |
| `crates/rc/src/handshake.rs` | `METHOD_REQS` 加 `config/get`、`config/unset`（均非阻断）；加 `proxy_capability()` |
| `crates/app/src/proxy.rs` | **新增**。`apply_to_remote`（真实下发）、`ApplyOutcome`、三条提示文案常量、`replay` 用的辅助函数 |
| `crates/app/src/config.rs` | `AppConfig` 加 `proxies` / `proxy_bindings`（`skip_serializing`，只落档案）+ `bind_proxy_slot` |
| `crates/app/src/profiles.rs` | 档案读写接入代理字段 |
| `crates/app/src/main.rs` | 7 个 `Cmd::Proxy*` 分支、`apply_proxy_column`、连接后补下发、删除 remote 级联清理、9 个 UI 回调 |
| `crates/app/ui/main.slint` | `RemoteRow` 加代理列字段、`ProxyRow` 结构、远程列表加「代理」列 + 下拉框、代理配置管理卡片、删除确认 |

### 7.2 三条硬要求的落地方式

| 要求 | 实现 |
|---|---|
| 不静默覆盖用户原有代理 | 覆盖前用 `remember_original` 把用户原值记进 `RemoteProxyBinding.original_override`；已有更早记录时沿用，不把自己写的值当原值（回滚链不断） |
| 切回「不代理」不误删原有设置 | `plan_set` 返回 `Restore(原值)` 而非 `Unset`；只有「原本没有这个键」才 `Unset`。`Some("")`（原本存在但为空）也恢复成空值而非删键 |
| 别人改过就报冲突 | 比较 `applied_value` 与 rclone 当前值，不一致 → `ProxyAction::Conflict`，**不做任何写入** |
| 不重启/不停止 rclone | 只调 `config/update` / `config/unset`，全程无进程操作 |
| 改代理需重新挂载/启动 | `RESTART_HINT` 常量，绑定时与连接后补下发时都会提示；**不自动重建** |

### 7.3 关于「用 URL 库」的偏离说明

需求要求「用 URL 解析/构造库处理地址」，实现时**没有**引入 `url` crate，原因：

- `url` 会规范化掉协议默认端口（http 的 80、https 的 443）并给 special scheme 补 `/`，
  导致用户填的端口在 `rclone.conf` 里凭空消失；
- 实测还发现 `url` 的 `set_password` **刻意不编码 `%`**，会生成 `...%ss` 这种非法转义。

改为按 RFC 3986 的 unreserved 集合（`A-Za-z0-9-._~`）之外全部百分号编码，
覆盖 `@ : / ? # % 空格` 全部特殊字符，IPv6 自动加方括号，行为可预测且与 Go 侧一致。
等价测试：`credentials_with_special_characters_are_percent_encoded`、
`userinfo_roundtrips_through_percent_decoding`、`ipv6_host_gets_brackets`。

### 7.4 验证命令与退出码

```powershell
cargo check -p rclone-gui          # 0 error, 0 warning          EXIT=0
cargo test --workspace             # 159 tests passed            EXIT=0
cargo build -p imagehost-plugin --target wasm32-wasip1 --release  EXIT=0
cargo build -p rclone-gui --release# 2m14s                      EXIT=0
python probe_proxy\e2e.py          # 9/9 PASS                    EXIT=0
```

测试分布：`rcgui-profile` 72、`rclone-gui` 38、`rcgui-rc` 22、`plugin_host` 24、其余 3。

### 7.5 端到端验证（真实 rclone，隔离实例）

`probe_proxy\e2e.py` 在 `127.0.0.1:5599` + 独立 conf 上验证 9 项，全部 PASS：

```text
[PASS] setting a proxy writes override.http_proxy
[PASS] switching back with no original UNSETS the key
[PASS] overwriting a user proxy records the original
[PASS] switching back RESTORES the user's original proxy (not unset)
[PASS] an external change is reported as a conflict, not overwritten
[PASS] a never-managed remote keeps its hand-set proxy
[PASS] no override.* residue left in rclone.conf
[PASS] SOCKS5 is written as override.http_proxy = socks5://…
[PASS] we never write override.socks_proxy (rclone ignores it)
```

第 4 项是本次最关键的安全性验证：用户原有 `http://user-corp-proxy:3128`
在我们覆盖并切回后被**还原**，而不是被删掉。

### 7.6 真实数据迁移验证

用 release 版启动，v1 档案自动升到 v2：

```json
"schema_version": 2,
"mounts": [...1 条...], "serves": [...2 条...], "remotes": [...1 条...],
"proxies": [], "remote_proxies": []
```

既有挂载/服务/远程全部保留，新字段补空。真实 remote `teldrive` 的键仍是
`api_host, api_key, channel_id, chunk_size, encoding, encrypt_files, hash_enabled,
page_size, type, upload_concurrency` —— **无任何 override.* 键**，确认没有误改。

### 7.7 本轮仍未验证

1. **UI 点击未能自动化验证** —— 本机 Slint 内容区点击送不进去（HANDOFF §六.4 已记录）。
   已改用两条替代证据：Slint 编译期类型检查通过（属性名/回调/结构全部核对），
   以及从 release 二进制中检出全部新 UI 文案（`代理配置`、`按远程分配代理`、`待应用`、
   `待修复`、`SOCKS5H`、`不等于强制直连` 均 FOUND）。
2. **ftp / sftp 的真实穿透**未验证（只证明选项能写入并落盘）。
3. **挂载（mount）侧的代理热生效**未单独测（serve 已测，预期一致）。
4. 代理连接失败时的错误文案未采样。

