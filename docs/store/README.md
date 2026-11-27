# RCGUI 插件商店

这个目录是商店页面的全部内容：一份索引（`index.json`）、一个静态页面（`index.html` + `style.css` + `app.js`），
以及这份作者指南。页面由 GitHub Pages 托管，公网地址：

- 索引：<https://h897105167.github.io/rclone_gui_windows/store/index.json>
- 页面：<https://h897105167.github.io/rclone_gui_windows/store/>

商店**不托管插件包**。每个插件仍然发布在作者自己的仓库 Release 上，
这里只记录"哪个插件、什么版本、从哪个地址取、sha256 是多少、需要多新的 RCGUI"。
所以这里没有上传功能，也没有账号系统——提交就是提 PR。
插件作者不必是维护者本人：任何 GitHub 账号的公开 Release 都能上架，
前提是包能下载、哈希对得上、人工审核过代码和权限（见下文"审核门槛"）。

## 索引格式

```json
{
  "schema_version": 1,
  "plugins": [
    {
      "id": "teldrive",
      "name": "Teldrive",
      "version": "0.3.0",
      "description": "一句话说明这个插件能做什么。",
      "repository": "https://github.com/h897105167/rcgui-plugin-teldrive",
      "url": "https://github.com/h897105167/rcgui-plugin-teldrive/releases/download/v0.3.0/teldrive-0.3.0.zip",
      "sha256": "<64 位小写十六进制>",
      "min_gui_version": "0.3.0",
      "capabilities": ["kv", "fs", "http", "proc", "net", "rclone", "env"]
    }
  ]
}
```

字段规则（`scripts/validate-store-index.ps1` 会逐条检查，CI 里也跑）：

| 字段 | 类型 | 要求 |
| --- | --- | --- |
| `id` | string | 小写字母、数字、连字符，2–32 位，不能以连字符开头或结尾；与 `config.json` 里的 `id` 一致；全表唯一 |
| `name` | string | 给人看的名字，非空，≤ 60 字。**必须是字符串**，不能写成本地化对象 `{"zh":…,"en":…}` |
| `version` | string | 语义化版本，形如 `1.2.3`；与 `config.json` 的 `version` 和 URL 里的 tag 一致 |
| `description` | string | 一句话说清做什么，10–300 字 |
| `repository` | string | `https://github.com/<owner>/<repo>`，owner 可以是任何 GitHub 账号（不等于维护者）；不要带 `.git`、query 或结尾斜杠 |
| `url` | string | `https://github.com/<owner>/<repo>/releases/download/v<version>/<id>-<version>.zip`。必须与 `repository` 是**同一个 owner/repo**；只许 https、端口只能是 443、不许 userinfo（`user:pass@`）、query（`?…`）或 fragment（`#…`）；必须是能直接下到的 zip |
| `sha256` | string | 该 zip 的 SHA-256，64 位小写十六进制；不能是占位符 |
| `min_gui_version` | string | 语义化版本：真正包含"能跑这个插件"的那个 RCGUI 版本 |
| `capabilities` | string[] | `config.json` 里声明的权限，只能取 `kv` `ui` `fs` `http` `proc` `net` `rclone` `env` |

`oneclick`（真实 id 是 `onerclone`）不进商店：它会自己拉起 rclone 进程，
和 RCGUI 0.3.0 起的受管内核会互相打架。它在便携包里也被排除。

## 作者怎么提交

1. **先发一个 issue**，用 [插件提交模板](https://github.com/h897105167/rclone_gui_windows/issues/new?template=plugin_submission.yml)。
   issue 里写清插件 id、版本、仓库、Release 资产地址、sha256、权限和许可证。
   issue 是审核的起点，不是上架动作。
2. **再提 PR，只改 `docs/store/index.json`**。加一条，或者改已有那条的字段。
   PR 里不要附带 zip、exe 或任何二进制。
3. 维护者按下文"审核门槛"核对后合并。合并即生效：页面读的就是这个文件。

自己先跑一遍：

```powershell
# 只做本地检查（schema、格式、与 plugins/manifest.json 交叉核对）
.\scripts\validate-store-index.ps1

# 连地址一起验证：真的下载每个 url 并比对 sha256
.\scripts\validate-store-index.ps1 -VerifyRemote
```

`-VerifyRemote` 会访问 GitHub Releases。你的地址如果只能内网访问、或者过一会儿就删，
那它就不适合进商店——别人装不上。

## 随程序发布和只在商店里

同一个插件可以有两条发布路径，互不替代：

- **随程序发布**：`plugins/manifest.json` 里锁一行（tag + 资产名 + sha256），构建时由
  `scripts/fetch_plugins.ps1` 下载校验、打进便携包，用户首次运行时释放出来。
- **只在商店里**：只写进本索引。用户装完程序后从商店页面或应用里点安装，应用按 `url`
  下载、比对 `sha256` 与 `min_gui_version` 后装进自己的插件目录。

两处都有时，指向的必须是同一个包（`sha256` 一致、清单 `tag` 等于 `v`+`version`），
校验脚本会核对。只在商店里的插件不必进清单：清单是"随 exe 分发什么"，不是"谁能上架"。

## 审核门槛

自动检查（PR 上跑 `store-review` workflow）能挡住的：

- JSON 能解析、`schema_version` 是 1、没有多余字段、必填字段齐全
- `id` / `version` / `sha256` / `min_gui_version` 的格式，`capabilities` 合法且不重复
- `id` 全表唯一、不在排除清单里
- `url` 是 https、和 `repository` 同 owner 同 repo、含 `releases/download/`、没有 userinfo/query/fragment、端口是 443，资产文件名与 `id`+`version` 对得上
- `sha256` 与 `plugins/manifest.json` 里固定的一致（如果这个插件也在清单里）
- 加 `-VerifyRemote` 时：地址真的能下载、下载到的文件哈希一致，并且包根目录 `config.json` 的 `id` / `version`（包里有 `min_gui_version` 时还有它）/ `capabilities` 与索引条目一致

自动检查挡不住的，靠人看：

- **代码**：插件源码公开可读，没有明显恶意行为（偷凭据、扫盘、往陌生地址传数据）
- **许可证**：插件仓库有明确许可证；没有许可证的插件不进商店
- **权限**：`capabilities` 与实现相符。声明了 `fs` 就得说清读写哪些目录；
  声明 `proc` 就得说清起什么进程。权限写得比实际小，是最常见的拒收理由
- **行为**：会往哪些域名发请求、会上传什么、会不会在后台常驻
- **`min_gui_version`**：必须是真的含这个插件能力的版本，不能为了让更多人装上而往低了写
- **换域名**：地址只接受 `https://github.com/<owner>/<repo>/releases/download/…`。
  想用别的托管（自建站、对象存储、网盘）要单独说服维护者，并且仍然要能被 `-VerifyRemote`
  直接下到、哈希核对通过。默认一律不收——"https 所以安全"不成立

明确拒收：占位条目（假哈希、`example.com`、空仓库）、自建下载站或网盘的地址、
需要额外安装器的包、与已上架插件同 id 的马甲包、把 oneclick 换个 id 塞进来的。

## 更新与下架

- **升版本**：改 `version`、`url`、`sha256`、必要时 `min_gui_version`，一条 PR。旧版本不留档——
  商店只列当前推荐的版本。
- **下架**：删掉那条，PR 里说明原因（漏洞、仓库删除、作者要求）。
  已经有用户装了的版本不会被卸载；下架只是不再推荐。
- **改名**：`id` 不要改。id 是插件数据的键，改了等于换一个插件。

## 隐私

页面是纯静态的：没有统计脚本、没有 cookie、没有第三方字体或 CDN。
唯一的网络请求是同目录的 `index.json`。点"下载 zip"会走到作者的 GitHub Release，
那一步是 GitHub 在记录访问，和本页面无关。

## 现在的状态

- 索引里目前是三个真实发布过的插件：`imagehost` 0.6.1、`kvault` 0.3.2、`teldrive` 0.3.0，
  三者的 sha256 都对着 GitHub Release 资产核过。
- `imagehost` 与 `teldrive` 同时在 `plugins/manifest.json` 里（随程序发布），
  `kvault` 只在商店里：它的面板要在用户打字时跟着刷新，这需要 RCGUI 0.3.2 起的宿主，
  所以 `min_gui_version` 写的是 0.3.2，0.3.1 及更早的构建会在安装前被拒。
- 应用里的商店页已经能用（列出、安装、更新、卸载）。页面上点"下载 zip"仍是手动路径，
  适合核对哈希或离线安装。
