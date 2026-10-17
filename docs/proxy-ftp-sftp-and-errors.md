# ftp / sftp 的代理穿透，以及代理失败的错误文案

> 本轮（2026-10-19）实测补充。上一轮 `docs/proxy-verification.md` 的结论不变，
> 这里只补它标着「未验证」的部分，并记录两个由此暴露的缺陷。

测试环境：rclone v1.75.4（amd64），Windows 11 25H2；所有服务都在 127.0.0.1 上，
配置与数据全部落在临时目录，**未读取也未修改用户的 `rclone.conf`**。

## 一、结论速览

| 目标 | 代理方式 | 结果 |
|---|---|---|
| 任意 HTTP 传输类 | `override.http_proxy = <url>` | ✅ 生效（上一轮已证） |
| ftp | `override.http_proxy` | ❌ **完全不生效**（静默直连） |
| sftp | `override.http_proxy` | ❌ **完全不生效**（静默直连） |
| ftp | 原生 `http_proxy = http://h:p` | ✅ 控制连接 + 被动数据连接都过代理 |
| ftp | 原生 `socks_proxy = h:p` | ✅ 同上 |
| sftp | 原生 `http_proxy` / `socks_proxy` | ✅ 生效 |

因此 **RCGUI 目前对 ftp / sftp 下发代理是无效的**，因为实现只写
`override.http_proxy` 一个键。这一点已在界面上明示（见第四节）。

## 二、验证方法（可复现）

`ui_probe/ftp_sftp_proxy_matrix.py`（仓库外脚本）：

1. 目标服务用 rclone 自己起，不依赖外部软件：
   - `rclone serve ftp <root> --addr 127.0.0.1:2121 --user U --pass P`
   - `rclone serve sftp <root> --addr 127.0.0.1:2022 --user U --pass P`
2. 见证器是**会真正转发**的 SOCKS5 与 HTTP CONNECT 代理，逐条记录
   `CONNECT host:port`；只做假握手的话只能证明「连上代理」，不能证明「穿透」。
3. 矩阵跑「直连 / SOCKS5 / HTTP 代理」× ftp、sftp，判定条件：
   **列表成功 且 见证器记录到了目标**。只成功没记录 = 静默绕过，判定失败。

实测输出（节选）：

```text
[PASS] ftp_direct      witness: (direct)
[PASS] ftp_socks       witness: CONNECT 127.0.0.1:2121 | CONNECT 127.0.0.1:31215
[PASS] ftp_http        witness: CONNECT 127.0.0.1:2121 | CONNECT 127.0.0.1:31191
[PASS] sftp_socks      witness: CONNECT 127.0.0.1:2022
[PASS] sftp_http       witness: CONNECT 127.0.0.1:2022
[FAIL] ftp_override_http   exit=0 listing_ok=True proxy_used=False  <no proxy traffic>
[FAIL] sftp_override_socks exit=0 listing_ok=True proxy_used=False  <no proxy traffic>
```

ftp 那两行的第二个 `CONNECT` 是被动模式的数据连接（端口由 PASV 分配），
说明**数据连接也确实走代理**，不只是控制连接。

## 三、两个必须记住的格式差异

1. **原生 `socks_proxy` 只认裸 `host:port`**。写成 `socks5://h:p` 会直接失败：

   ```text
   CRITICAL: NewFs: failed to make FTP connection to "127.0.0.1:2121":
   socks connect tcp: dial tcp: address socks5://127.0.0.1:57980: too many colons in address
   ```

   协议靠「值是 socks_proxy」这件事本身决定，不需要 scheme。

2. **原生 `http_proxy` 要完整 URL**（`http://h:p`），与 `override.http_proxy` 一致。

3. 写进 `rclone.conf` 的 `pass` 必须是 obscured 值；直接写明文会报
   `input too short when revealing password - is it obscured?`（脚本里用
   `rclone obscure` 生成）。

## 四、产品侧处置

### 4.1 上一轮的临时措辞（已移除）

`type_honors_override()` / `INERT_TYPE_HINT` 曾用于提示「ftp / sftp 不会生效」。
2026-10-18 随下面的正式支持一起删除 —— 再保留会让用户以为仍然做不到。

### 4.2 正式支持（2026-10-18 会话 6）

按类型选键，逻辑集中在 `crates/profile/src/proxy.rs`：

| 远程类型 | 协议 | 键 | 值 |
|---|---|---|---|
| 其它 | 任意 | `override.http_proxy` | 完整 URL（含 scheme） |
| ftp / sftp | http / https | `http_proxy` | 完整 URL |
| ftp / sftp | socks5 / socks5h | `socks_proxy` | 裸 `[user[:pass]@]host:port` |

规则（用户 2026-10-18 决定）：

1. **原生键只补空缺**：ftp / sftp 上若用户已写了 `http_proxy` 或 `socks_proxy`，
   RCGUI 不覆盖、不代管，只提示「你在 rclone 里已经设置了……」并计入「跳过」。
2. **socks_proxy 优先**：两个原生键同时存在时 rclone 用 socks；RCGUI 写
   `http_proxy` 前会检测并说明，不会写一个「看起来成功、实际不生效」的值。
3. **回滚记住键**：绑定新增 `applied_key`，切协议/切类型时先还原旧键，再写新键。
4. **凭据限制**：原生 `socks_proxy` 不做 URL 解码，因此用户名不能含 `:`、
   账号密码不能含 `@`（模型层直接拒绝并给中文说明）；`http_proxy` 的 Basic
   认证由 rclone 用 `Userinfo.String()` 生成，含特殊字符时可能认证失败
   （rclone 侧行为，RCGUI 未改）。
5. `socks5` 与 `socks5h` 在原生键上**等价**：x/net/proxy 总是把域名交给代理解析。

未实测（本轮只做单测与编译验证）：

- ftp / sftp 在 RCGUI 内的真实穿透端到端复测（上一轮只验证到 rclone 层面）；
- `https://` 指向「TLS 到代理」的原生 `http_proxy`（rclone 源码支持，未实测）；
- 含特殊字符的凭据在原生 `http_proxy` 上的认证行为。

## 五、代理失败的错误文案（实测样本）

`ui_probe/sample_proxy_errors.py` 抓到的原文（rclone v1.75.4）。这些都是**判定依据**，
`crates/app/src/proxy.rs` 的单测直接引用了同样的字符串：

| 场景 | 原文关键片段 |
|---|---|
| HTTP 代理端口没监听 | `proxyconnect tcp: dial tcp 127.0.0.1:9: connectex: No connection could be made because the target machine actively refused it.` |
| SOCKS5 代理端口没监听 | 同上（与 HTTP 形态一致，只能靠配置区分） |
| 代理要求认证 | `StatusCode: 407 ... api error ProxyAuthenticationRequired: Proxy Authentication Required` |
| 代理网关上游不可达 | `StatusCode: 502 ... api error BadGateway: Bad Gateway` |
| `https://` 指向纯 HTTP 代理 | `proxyconnect tcp: tls: first record does not look like a TLS handshake` |
| 代理接受连接但不响应 | `request send failed, Get "...": net/http: timeout awaiting response headers` |
| 代理域名解析失败 | `request send failed, Get "...": EOF` |

RC 侧拿到的形状：

```json
{"error":"error in ListJSON: operation error S3: ListBuckets, ... proxyconnect tcp: ... actively refused it.","path":"operations/list","status":500}
```

实现取舍：**只在原文出现明确代理证据时归因给代理**（`proxyconnect` / 407 /
`ProxyAuthenticationRequired` / `BadGateway`），超时与 `EOF` 这类文本对「远端不可达」
同样成立，不做强归因，只在「该远程确实绑了代理」时给一句弱提示。
目标站点自己拒绝连接（无 `proxyconnect`）必须原样透出 —— 有单测锁定。

## 六、本轮由此修掉的两个缺陷

1. **删除代理配置后没有回滚 rclone**（严重）。确认框承诺「这些远程的代理会被恢复
   成原状」，但实现里先 `save_from_cfg` 删掉了绑定，再想从档案里读回
   `original_override` 已经读不到，当时的 `plan_set` 判定「不是本程序管的」→ 一个字节都不写。
   现象：rclone.conf 里永久留着一个已被删除的代理地址。修法：删除前先把受影响的
   绑定克隆出来，回滚用克隆记录。
2. **给 ftp / sftp 下发代理静默无效**（当时改为显式提示；2026-10-18 已按第四节
   改写成原生键，提示随 `INERT_TYPE_HINT` 一起删除）。
