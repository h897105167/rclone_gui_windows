# RCGUI 插件开发与接入规范

本文是 RCGUI 插件体系的**唯一契约**。它规定了：

1. 一个插件长什么样、放在哪、怎么被安装和卸载；
2. 插件的界面从哪里来（声明式面板），逻辑怎么跑（WASM）；
3. 本体 ↔ 插件之间的通信协议、能力授权与错误约定；
4. 本仓库（本体）如何接入插件：仓库分离、清单文件、CI 拉取与校验；
5. 命名规则、版本兼容承诺、调试方法与安全边界。

> 一句话原则：**本体不认识任何具体插件。** 本体里不允许出现插件的 id、
> 插件的界面代码、插件的业务逻辑。新增一个插件，只需要发布一个新的插件仓库，
> 不需要改本体源码。

---

## 0. 快速开始

```powershell
# 1. 新建插件仓库（不要 fork 本体）
git clone https://github.com/<you>/rcgui-plugin-hello
cd rcgui-plugin-hello

# 2. 用模板（见 §9）写 src/main.rs + config.json
# 3. 本地构建
cargo build --release --target wasm32-wasip1

# 4. 打包：config.json + main.wasm + check.sha256 → zip
pwsh ./scripts/pack.ps1

# 5. 打 tag 发 Release，把 zip 挂上去
git tag v0.1.0; git push --tags
```

然后在本体的 `plugins/manifest.json` 里加一条记录（§7.2），本体下一次构建就会
自动拉取、校验并内置这个插件。

---

## 1. 总体架构

```
┌───────────────────────────── 本体 rclone_gui_windows ─────────────────────────────┐
│  crates/app            界面（Slint）+ 业务 + 能力实现（caps.rs）                    │
│  crates/plugin         plugin_host：WASM 运行时、协议、包格式、校验                │
│  crates/rc rclone …    rclone RC 客户端等基础设施                                   │
│                                                                                   │
│  plugins/manifest.json ← 唯一和"具体插件"有关的东西：id / repo / tag / sha256      │
│  crates/app/assets/plugins/<dir>/  ← 构建期由 scripts/fetch_plugins.ps1 拉下来的包  │
└───────────────────────────────────────────────────────────────────────────────────┘
                      ▲ 协议（stdin/stdout JSON）        ▲ 声明式面板（config.json）
                      │                                  │
        ┌─────────────┴──────────┐        ┌──────────────┴───────────┐
        │ rcgui-plugin-hello     │        │ rcgui-plugin-oneclick    │  … 每插件一仓库
        │ config.json            │        │ config.json              │
        │ check.sha256           │        │ check.sha256             │
        │ main.wasm              │        │ main.wasm                │
        └────────────────────────┘        └──────────────────────────┘
```

三层职责，互不越界：

| 层 | 位置 | 职责 | 禁止 |
|---|---|---|---|
| **本体** | 本仓库 | 运行时、能力、面板渲染器、插件管理页 | 出现插件 id、插件界面、插件业务逻辑 |
| **插件包** | 插件仓库 Release | 元数据 + 声明式界面 + WASM 逻辑 | 直接访问网络/文件/进程（只能通过宿主能力） |
| **分发清单** | `plugins/manifest.json` | 插件从哪来、什么版本、校验和 | 存插件源码 |

插件的 **UI 也是插件的一部分**：插件页面上看到的每一行、每一个按钮，
都来自该插件 `config.json` 的 `plugin_setting.controls`。本体只提供
一个通用的控件渲染器。

---

## 2. 插件包格式

插件是一个**目录**（发布时打成 zip），标准三件套：

```
myplugin/
  config.json      # 元数据 + 声明式界面
  check.sha256     # 完整性校验表
  main.wasm        # 入口程序（WASI preview1 命令）
```

### 2.1 `config.json`

```jsonc
{
  "id": "hello",                     // 必填。全局唯一，小写字母/数字/连字符，见 §3.1
  "name": "示例插件",                 // 必填。插件列表显示名
  "version": "0.1.0",                // 必填。语义化版本，升级判定靠它
  "entry": "main.wasm",              // 可选。入口文件名，默认 main.wasm
  "description": "一句话说明",        // 可选。插件卡片副标题
  "author": "你的名字",               // 可选
  "tags": ["示例", "hello"],          // 可选。插件列表检索用
  "platform": "any",                 // 可选。any / windows，见 §3.4
  "builtin": false,                  // 可选。true 表示"随本体分发"，见 §6.4
  "http_entry": false,               // 可选。true 表示接管内嵌 HTTP 路由，见 §5.7
  "capabilities": ["kv", "ui"],      // 必填（可为空数组）。能力组，见 §5
  "plugin_setting": {                // 可选。插件页的声明式界面，见 §4
    "version": 1,
    "summary": "一句话介绍这个插件能干什么。",
    "on_start": "get",               // 可选。打开插件页时自动执行的动作 key
    "on_exit": "exit",               // 可选。仅登记，不调用 WASM（见 §4.5）
    "controls": [ /* 见 §4.1 */ ]
  }
}
```

字段全部宽松解析：未知字段会被忽略（便于向后兼容），缺失的可选字段取默认值。
`id` / `name` / `version` 缺失时插件不会被加载。

### 2.2 `check.sha256`

纯文本，每行 `sha256  相对路径`（两个空格分隔，与 `sha256sum -c` 同格式）：

```
9f2c…  config.json
4a71…  main.wasm
```

规则（实现见 `crates/plugin/src/bundle.rs::verify_dir`）：

- **入口文件必须在表里**，否则视为包损坏；
- 表里列出的每个文件都要存在且哈希一致；
- **允许包里存在表外的文件**（例如运行时生成的 `kv.json`），它们不参与校验。

生成方式（打包脚本里最后一步，见 §8.2）：

```powershell
$files = 'config.json', $entry
$lines = $files | ForEach-Object {
  '{0}  {1}' -f (Get-FileHash $_ -Algorithm SHA256).Hash.ToLower(), $_
}
$lines -join "`n" | Set-Content -NoNewline -Encoding ascii check.sha256
```

> 历史兼容：`plugin.json` / `manifest.sha256` 是上一代的文件名，宿主仍能读取，
> 但新插件一律用 `config.json` / `check.sha256`。

---

## 3. 命名规则

命名是接口的一部分：插件名、目录名、Release 资产名、动作 key、能力名
必须严格遵守，否则本体认不出来。

### 3.1 插件 id

| 规则 | 说明 |
|---|---|
| 字符集 | `[a-z0-9-]`，首字符为字母 |
| 长度 | 2–32 |
| 唯一性 | 全局唯一，发布后**不得更改**（改了等于换一个插件，配置和 kv 数据会丢） |
| 反例 | `Hello`、`hello_plugin`、`com.you.hello` |

id 决定的东西：kv 存储目录、`config.json` 的 `id`、本体的安装目录名、
日志前缀。**一旦发布就冻结。**

### 3.2 仓库名与目录名

| 对象 | 规则 | 例 |
|---|---|---|
| 插件仓库 | `rcgui-plugin-<name>` | `rcgui-plugin-oneclick` |
| 本体内目录 | 与插件 id 一致 | `assets/plugins/hello/` |
| Release 资产 | `<id>-<version>.zip` | `hello-0.1.0.zip` |
| Release tag | `v<version>` | `v0.1.0` |

`<name>` 是仓库名里的可读部分，可以和 id 不同（`rcgui-plugin-oneclick` 的 id 是
`onerclone`），但**建议一致**，除非 id 已经被历史版本占用。

### 3.3 动作 key（`action` 控件）

| 规则 | 说明 |
|---|---|
| 字符集 | `[a-z0-9_-]`，建议小写短横线 |
| 唯一性 | 同一 `plugin_setting` 内唯一（`divider` 的 key 可重复） |
| 语义 | **本体不理解动作的含义**，只把它原样放进 `params.action` 发给插件 |
| 例 | `save`、`test-conn`、`refresh`、`open-dir`、`copy-link` |

`on_start` / `on_exit` 里写的也是动作 key。宿主对 `on_exit` **只登记不调用**
（§4.5）。

### 3.4 平台标识

`platform` 取值：

| 值 | 含义 |
|---|---|
| `any` / 留空（默认） | 任何平台都加载 |
| `windows` | 仅在 Windows 上加载（macOS/Linux 上隐藏） |

### 3.5 能力组名

见 §5.1，只能写那 8 个组名之一；写错会在加载时被拒绝（未知能力组 =
插件永远拿不到该能力，等于静默失败，所以宿主在装载时直接报错更有用）。

---

## 4. 声明式面板（插件 UI）

这是"插件 UI 与本体分离"的核心：**本体不写任何插件专属界面代码**，
插件的整页 UI 由 `plugin_setting` 描述，由本体通用渲染器画出来。

### 4.1 控件

`controls` 是数组，按顺序从上到下排列。每个控件：

| 字段 | 类型 | 适用 kind | 说明 |
|---|---|---|---|
| `kind` | string | 全部 | `group` `field` `action` `checks` `status` `hint` `progress` `divider` |
| `key` | string | 除 `divider` | 唯一标识。`field` 的 key 就是表单字段名 |
| `label` | string | all | 显示文字；`action` 必填（按钮文字） |
| `text` | string | hint / status / checks | 正文；`hint` 支持多行，`checks` 里是"清单为空时的提示" |
| `placeholder` | string | field | 输入提示 |
| `field` | string | field | `text` `password` `number` `bool` `select` `path` |
| `options` | string[] | field(select) | 下拉选项；**必填** |
| `style` | string | action | `secondary`（默认）/ `primary` / `danger` |
| `line` | string | field / action | 同一 `line` 非空值的控件排在**同一行** |
| `width` | number | field | 像素宽度，0 = 自适应 |
| `lamp` | bool | status | 是否在这一行显示指示灯；颜色由返回值 `lamp`（`ok`/`warn`/`err`）决定 |
| `dialog_title` | string | action | 若动作会弹文件选择框，标题用它 |
| `filters` | string[] | action | 文件扩展名过滤，如 `["png","jpg"]` |
| `default` | string | field | 默认值；`bool` 用 `"true"` / `"false"` |
| `enabled` | bool | 全部 | 初始是否可用（可被返回值覆盖） |

`kind` 一览：

| kind | 渲染为 | 说明 |
|---|---|---|
| `group` | 分组标题 + 分隔线 | 纯视觉 |
| `field` | 输入框 / 密码框 / 数字 / 开关 / 下拉 / 路径（带「浏览」） | 值会随动作提交给插件 |
| `action` | 按钮 | 点击 → `call_plugin(id, key, form)` |
| `checks` | ✓/✗ 清单 | 内容由返回值 `checks` 提供 |
| `status` | 一行状态 + 指示灯 | 内容由返回值 `status` + `lamp` 提供 |
| `hint` | 说明文字 | 可被返回值 `hints[key]` 覆盖 |
| `progress` | 进度条 | 由返回值 `progress` 驱动（负数 = 不确定态） |
| `divider` | 纯分隔线 | 不需要 key |

### 4.2 布局示例

```jsonc
"controls": [
  { "kind": "group", "key": "g-conn", "label": "连接" },
  { "kind": "field", "key": "host", "label": "地址", "placeholder": "http://127.0.0.1",
    "field": "text", "line": "row1", "width": 220 },
  { "kind": "field", "key": "port", "label": "端口", "field": "number",
    "default": "6800", "line": "row1", "width": 90 },
  { "kind": "action", "key": "test-conn", "label": "测试连接", "style": "primary", "line": "row1" },
  { "kind": "status", "key": "conn-state", "label": "状态" },
  { "kind": "divider", "key": "d1" },
  { "kind": "group", "key": "g-dirs", "label": "目录" },
  { "kind": "field", "key": "local", "label": "本地", "field": "path", "line": "row2" },
  { "kind": "field", "key": "new", "label": "新建子目录", "field": "text", "line": "row2" },
  { "kind": "action", "key": "mkdir", "label": "创建", "line": "row2" },
  { "kind": "checks", "key": "precheck", "label": "环境检查" },
  { "kind": "hint", "key": "tip", "text": "提示：路径里不要带空格。" }
]
```

### 4.3 动作调用

点按钮时本体执行：

```
call_plugin(ctx, id, action_key, form) → 方法 "request"，params 至少含：
{ "action": "test-conn", "form": { "host": "...", "port": "6800" }, "live": {...} }
```

插件返回的 `result.body` 可以用这些键驱动界面：

| 键 | 类型 | 作用 |
|---|---|---|
| `values` | object | 回填字段值（`{"host": "..."}`） |
| `checks` | array | `[{"label":"rclone 已连接","value":"…","ok":true}]` |
| `status` | string | 状态行文字 |
| `lamp` | string | 覆盖状态指示灯 `ok` / `warn` / `err` |
| `hints` | object | 覆盖某条 `hint` 的文字 |
| `enabled` | object | 启用/禁用控件 `{"save": false}` |
| `progress` | number | 0–100；负数 = 不确定态进度条 |
| `toast` | object | 主动弹通知 `{"level":"info|warn|err","message":"…"}` |
| `rc` | object | 连接信息 `{"url","user","pass","connect":true}`，本体据此接管主界面的连接状态 |

`result.status`（HTTP 风格状态码，通常 200）与 `result.body` 一起返回；
`status >= 400` 时本体按错误处理（红字 toast + 不进 `values`）。

### 4.4 表单与默认值

- 字段值统一以**字符串**提交（包括 `number` / `bool`：`"true"`/`"false"`）。
  插件自己解析，避免 JSON 类型在两端的歧义。
- 首次打开插件页时，本体用 `default` 初始化表单；随后以 kv 里保存的值为准
  （插件自己在 `save` 动作里写 kv）。
- 表单是**整页**提交的：每次动作都带上当前所有字段值。

### 4.5 `on_start` / `on_exit`

| 字段 | 行为 |
|---|---|
| `on_start` | 打开插件页时以该动作调用一次 wasm（等同用户点了这个按钮） |
| `on_exit` | **只登记，不调用 wasm**。本体退出时不得等待插件（否则关不掉的插件会拖死退出流程）。想做清理的，在 `on_start` 或自己的 `save` 里做 |

---

## 5. 通信协议

### 5.1 形态

插件是 **WASI preview1 命令**：宿主启动一次、写一个 JSON 到 stdin、读一个 JSON
从 stdout、进程退出。**没有常驻状态**——需要跨轮次的数据用 `state` 往返传递，
需要持久化的用 `kv.*` 能力。

```
宿主 ──stdin: {"id":1,"method":"manifest"}──────────────────────────▶ wasm
宿主 ◀─stdout: {"id":1,"manifest":{"id":"hello","name":"…","version":"0.1.0"}}──

宿主 ──stdin: {"id":2,"method":"request","params":{"action":"get",…}}─▶ wasm
宿主 ◀─stdout: {"id":2,"need":[{"cap":"kv.get_all","key":"cfg"}],"state":{…}}──
宿主   （执行 need，把结果放进 provided）
宿主 ──stdin: {…,"params":{"provided":{"cfg":{…}},"state":{…}}}──────▶ wasm
宿主 ◀─stdout: {"id":2,"result":{"status":200,"body":{…}},"ops":[{…}]}──
```

### 5.2 宿主 → 插件

| 字段 | 说明 |
|---|---|
| `id` | 轮次编号，插件必须原样回传 |
| `method` | `manifest`（询问元数据）/ `request`（执行业务） |
| `params.action` | 动作 key（`on_start` 或用户点击的按钮） |
| `params.form` | 当前表单值（字符串键值对） |
| `params.provided` | 上一轮 `need` 的执行结果，键是 `need[].key` |
| `params.state` | 上一轮插件自己回传的 `state`，原样送回 |
| `params.live` | 宿主当前长任务进度 `{"busy","label","done","total"}`，无需 need |
| `params.now` | 宿主时间与版本等环境信息 |
| `params.req_id` | 本次调用的唯一 id，便于日志关联 |
| `params.http` | 仅 HTTP 入口调用时出现：这次请求的快照，见 §5.7 |

### 5.3 插件 → 宿主

四种响应，四选一：

```jsonc
// 1) 元数据（仅 method=manifest）
{ "id": 1, "manifest": { "id": "hello", "name": "示例", "version": "0.1.0", "description": "…" } }

// 2) 需要宿主能力：先要东西，下一轮再算
{ "id": 2, "need": [ { "cap": "kv.get_all", "key": "cfg", "params": {} } ], "state": { "step": 1 } }

// 3) 完成：要执行的副作用 + 返回给界面的结果
{ "id": 2,
  "ops": [ { "op": "kv.set", "key": "cfg", "value": { "host": "127.0.0.1" } },
           { "op": "ui.toast", "level": "info", "message": "已保存" } ],
  "result": { "status": 200, "body": { "status": "已保存", "lamp": "ok" } } }

// 4) 出错
{ "id": 2, "error": { "message": "缺少 host 参数" } }
```

`error` 会被本体显示为插件错误（带上插件 id），不会让界面崩掉。

### 5.4 轮次与资源限制

| 限制 | 值 | 说明 |
|---|---|---|
| 协商轮数 | 16 | `need` → `provided` 往返最多 16 轮，超了判失败 |
| fuel | 5×10^10 | 单次调用的 CPU 预算，防止死循环卡死界面 |
| 栈 | 16 MB | WASI 线程栈 |
| 超时 | 宿主侧 | 超过即终止该次调用并报错 |
| 模块缓存 | mtime 键 | wasm 文件变了就重新编译 |

`need` 里的能力会**先做授权检查再执行**：未在 `capabilities` 里声明的组，
宿主直接拒绝并回错误（`插件未在 config.json 的 capabilities 里声明「http」，拒绝执行 http.download`）。

### 5.5 `need` 的键与顺序

- `key` 是插件自己起的名字，`provided[key]` 就是该能力的返回值；
- 同一轮多个 `need` 会被并行执行；
- 需要"先查再决定"的，分轮做：第一轮 `need` 查询，第二轮根据 `provided` 再 `need`。

### 5.6 `ops` 的语义

`ops` 是"这次要留下的副作用"，由宿主在返回 `result` **之前**依次执行：

| op | 参数 | 说明 |
|---|---|---|
| `kv.set` | `key`, `value` | 写插件私有 kv |
| `kv.delete` | `key` | 删键 |
| `ui.toast` | `level`, `message` | 弹通知（不阻塞） |
| `rclone.upload_file` | `fs`, `remote_path`, `filename`, `data_b64` | 上传到 rclone 远程 |

需要**读**数据的，一律走 `need`；`ops` 只放写入类动作，这样重试时不会重复读。

### 5.7 内嵌 HTTP 入口（`http_entry`）

本体自带一个 HTTP 服务（端口在设置页配）。它**只认路由前缀，不认业务路径**：

| 路由 | 行为 |
|---|---|
| `GET /api/health` | 探活，固定返回宿主状态 |
| `ANY /plugin/{id}`、`ANY /plugin/{id}/{*rest}` | 转给 `config.json` 里 `http_entry: true` 的插件 |
| `ANY /api/plugin/{id}/{*rest}` | 旧别名，等价于上一条（保留兼容） |

声明 `http_entry: true` 后，一次请求会以 `method: "request"` 调起该插件，
`params.http` 就是这次请求的完整快照：

| 字段 | 说明 |
|---|---|
| `method` | `GET` / `POST` / …（大写） |
| `path` | **已去掉前缀的**剩余路径，如 `/plugin/imagehost/upload` → `/upload` |
| `query` | 查询串，`{键: 值}`（同名参数只留最后一个） |
| `headers` | 除 `authorization` / `host` / `content-length` 外的全部请求头，键小写 |
| `authorization` | 原始 `Authorization` 头（鉴权由插件自己判） |
| `body_b64` | 原始请求体，base64；`multipart/form-data` 时为空串 |
| `multipart` | 仅 `multipart/form-data`：`{"fields":{…},"files":[{"filename","dataB64"}]}` |
| `origin` | 由 `Host` 拼出的源，如 `http://127.0.0.1:8737`，插件可用它拼回绝对链接 |
| `remote_addr` | `X-Forwarded-For` 优先，否则 `127.0.0.1` |
| `req_id` | 本次请求的唯一 id，便于和日志对上 |

插件照常回 `result`，本体会把它翻成 HTTP 响应：

| 字段 | 说明 |
|---|---|
| `status` | HTTP 状态码，缺省 200 |
| `body` | JSON 响应体（`application/json`） |
| `body_b64` + `content_type` | 二进制响应（图片、文件流）；给了它就忽略 `body` |
| `headers` | 附加响应头 `{键: 值}` |

请求体上限 100 MB（`BODY_LIMIT`），超了直接 400。
插件未启用时返回 403 并提示去「插件」页启用；插件报错时返回 500，错误文本带插件 id。

> 这套东西是**通用**的：宿主里没有任何插件 id 或业务路径。图床的 `/upload`、`/i/…`、
> `/s/…` 全部在 `rcgui-plugin-imagehost` 自己的 `match path` 里。

---

## 6. 宿主能力（Capability）

插件自己不能碰网络、文件、进程——只能"申请"。这是安全边界，也是跨平台的基础。

### 6.1 能力组

`capabilities` 里只能写这 8 个组：

| 组 | 授权的操作 | 用途 |
|---|---|---|
| `kv` | `kv.get` `kv.set` `kv.delete` `kv.get_all` | 插件私有键值存储 |
| `ui` | `ui.toast` `ui.confirm` `ui.copy` | 通知、确认框、写剪贴板 |
| `fs` | `fs.meta` `fs.sha256` `fs.read_text` `fs.read_b64` `fs.write_text` `fs.copy` `fs.mkdir` `fs.remove` `fs.list_dir` `fs.find` `fs.unzip` `fs.pick_file` `fs.pick_dir` `fs.open_dir` `fs.pe_info` `fs.signature` `fs.search_exe` | 文件与可执行文件探测 |
| `http` | `http.download` `http.request` `http.text` `http.json` | 下载与请求 |
| `proc` | `proc.start` `proc.stop` `proc.status` `proc.run` | 子进程 |
| `net` | `net.port_open` | 端口探测 |
| `rclone` | `rclone.call` `rclone.upload_file` `rclone.cat` | 调用已连接的 rclone RC |
| `env` | `env.info` | 宿主版本、架构、是否提权 |

单条操作到组的映射表是 `crates/plugin/src/config.rs::cap_of_op`，本体调用侧
`cap_call` 的 match 臂必须与它一致。

### 6.2 常用操作签名

```jsonc
// fs.meta     { path }                                   → { exists, is_dir, size, mtime }
// fs.read_text{ path }                                   → { text }
// fs.read_b64 { path, max_bytes? }                        → { data_b64, size }
// fs.write_text{ path, text, append? }                    → { bytes }
// fs.list_dir { path, recursive? }                        → { items: [{ name, path, is_dir, size }] }
// fs.find     { root, pattern, max? }                     → { items: [...] }
// fs.unzip    { src, dest }                               → { files }
// fs.pick_file{ title?, filters? }                        → { path }  (用户取消为 null)
// http.download{ url, dest, max_bytes? }                  → { path, bytes, sha256 }
// http.request { method?, url, headers?, body?, timeout_ms?, max_bytes? }
//                                                          → { status, content_type, body, size }
// proc.run    { program, args, timeout_ms? }              → { code, stdout, stderr }
// proc.start  { program, args, cwd? }                     → { pid }
// proc.status { pid }                                     → { running, code? }
// proc.stop   { pid }                                     → { stopped }
// net.port_open{ host, port, timeout_ms? }                → { open }
// env.info    {}                                          → { version, os, arch, elevated }
// kv.get_all  { }                                         → { … }  该插件的全部 kv
// kv.set      { key, value } / kv.delete { key }
// ui.confirm  { title, message }                          → { ok }
// ui.copy     { text }                                    → { ok }  写系统剪贴板
// rclone.call { method, params }                          → rclone RC 返回值
// rclone.upload_file { fs, remote_path, filename, data_b64 }
// rclone.cat  { remote, path, max_bytes? }                → { data_b64, size }  （走临时文件，见下）
```

早期命名 `fs.inspect_exe` / `fs.write_file` / `fs.remove_dir` 仍作为别名保留，
新插件不要用。

`rclone.cat` 内部并不是 `operations/cat`（部分 rclone 版本没有这个接口，实测 404），
而是 `operations/copyfile` 到 `<data>/tmp/cat-<pid>/` 再读回本地，读完即删；
所以它适合取几 MB 以内的对象（头像、配置文件），不适合大文件。

### 6.3 kv 存储位置

```
<用户数据目录>/plugins/<插件 id>/kv.json
```

- 只有该插件能读写自己的 kv；
- 卸载插件时 kv 一并删除（这是"干净卸载"的定义）；
- kv 是普通 JSON，可以手工检查/备份。

### 6.4 `builtin`

`builtin: true` 表示"随本体分发"（首次启动时从 `assets/plugins/` 复制到用户目录）。
非 builtin 的插件由用户手工安装。这两个标记只影响**管理页的展示与恢复**
（`ships_with_app` / `removed_builtins` 的墓碑逻辑），不影响运行。

---

## 7. 本体接入规则

这是"本体如何接一个新插件"的操作手册。**本体侧永远只有两类改动**：
清单里加一条、CI 里加一个拉取步骤（且后者是一次性的）。

### 7.1 铁律

| 规则 | 违反的样子 |
|---|---|
| 本体源码不得出现插件 id 字符串 | `if p.id == "imagehost"` 式的分支 |
| 本体不得 import 插件 crate | `[workspace] members` 里出现 `plugins/*` |
| 本体不得内嵌插件产物 | `include_bytes!(env!("…_WASM"))` |
| 本体不得写插件专属界面 | `main.slint` 里 `if p.id == "xxx"` 大段布局 |
| 本体的能力层不得为某插件开专线 | `http.rs` 里 `dispatch_to_plugin(st, "imagehost", …)` |
| 本体只允许通过通用协议、通用能力、声明式面板与插件交互 | — |

### 7.2 `plugins/manifest.json`

本体里唯一和具体插件有关的东西：

```jsonc
{
  "schema": 1,
  "body_repo": "https://github.com/h897105167/rclone_gui_windows",
  "plugins": [
    {
      "id": "onerclone",
      "name": "一键 rclone",
      "repo": "https://github.com/h897105167/rcgui-plugin-oneclick",
      "tag": "v0.3.1",
      "asset": "onerclone-0.3.1.zip",
      "sha256": "<zip 的 sha256>",
      "dir": "onerclone"
    }
  ]
}
```

| 字段 | 说明 |
|---|---|
| `schema` | 清单格式版本，当前 `1` |
| `body_repo` | 本体仓库地址（各插件 README 反向引用它） |
| `id` | 必须与插件包内 `config.json` 的 `id` 一致，否则拉取脚本报错 |
| `repo` | 插件仓库地址（公开、可匿名 clone） |
| `tag` | Release tag，固定为 `v<version>` |
| `asset` | Release 里那个 zip 的文件名 |
| `sha256` | zip 的哈希；**发布新版本时必须更新**，脚本会校验 |
| `dir` | 解包到 `crates/app/assets/plugins/<dir>/` 的目录名，通常等于 id |

> ⚠️ **`sha256` 必须取 Release 上那个 zip 的哈希，不是本地 `dist/` 里那个。**
> zip 里含文件时间戳，同一份源码在 CI 与本机打出来的包哈希不同。正确做法：
> 发完 Release 后 `gh release download <tag> -R <repo> && sha256sum *.zip`，
> 或者 `gh release view <tag> -R <repo> --json assets -q '.assets[].name'` 之后下载核对。

### 7.3 构建期拉取

本体仓库**不含任何插件源码与产物**。构建前执行：

```powershell
pwsh ./scripts/fetch_plugins.ps1            # 默认拉全部（同版本已就绪则跳过）
pwsh ./scripts/fetch_plugins.ps1 -Id onerclone   # 只拉一个
pwsh ./scripts/fetch_plugins.ps1 -Force     # 全部重下（换了包但版本号没变时用）
pwsh ./scripts/fetch_plugins.ps1 -Offline   # 只检查本地资产齐不齐（ci / 没网）
```

脚本行为（`scripts/fetch_plugins.ps1`）：

1. 读 `plugins/manifest.json`；
2. 逐项检查 `<dir>/.fetched.json`，tag 与 sha256 都没变 → 跳过；
3. 否则下载 `https://github.com/<owner>/<repo>/releases/download/<tag>/<asset>`；
4. **第一层校验**：zip 的 sha256 必须等于清单里的值（不符 → 报错，不静默降级）；
5. **第二层校验**：解包后按包内 `check.sha256` 逐文件核对（入口必须出现在表里）；
6. 先解到临时目录再整体替换 `crates/app/assets/plugins/<dir>/`，失败不会留下半套文件；
7. 写 `<dir>/.fetched.json`（id / tag / asset / sha256 / repo / fetched 时间）；
8. 任一插件失败 → 非零退出，并列出失败原因（缺网、Release 被删、哈希不符……）。

环境变量 `RCGUI_SKIP_PLUGINS=1` 等价于 `-Offline`。

`crates/app/build.rs` 只做两件事：编译 Slint、把 `assets/plugins/` 复制到输出目录。
**它不再为任何插件设置环境变量**（旧的 `IMAGEHOST_WASM` 已移除）。

### 7.4 CI

`.github/workflows/ci.yml` 的顺序：

```
checkout → rust toolchain(stable) → cargo cache
  → pwsh ./scripts/fetch_plugins.ps1      # 拉 Release 包，不编译插件源码
  → cargo test --workspace
  → ./scripts/build.ps1 -Release -SkipPlugins   # 资产上一步已就绪
  → upload artifact（exe + assets/plugins/**）
```

> 本仓库以前在 CI 里 `cargo build -p imagehost-plugin --target wasm32-wasip1`，
> 那是因为插件源码曾经放在 `plugins/` 下。分离后这段必须删掉——否则等于
> 本体仓库仍然持有插件源码。

### 7.5 版本与兼容

| 约定 | 说明 |
|---|---|
| 协议版本 | 当前 `1`（`params.protocol` 由宿主传入，插件可据此降级） |
| 能力只增不改 | 已有 op 的参数语义不破坏；废弃 op 至少保留两个小版本 |
| 面板契约 | 新增 `kind` 前先在宿主实现并容忍未知 kind（本体遇到未知控件跳过，不报错） |
| 插件升级 | 本体按 `config.json.version` 判断；内置包版本更高时覆盖用户目录里的旧包（kv 保留） |

### 7.6 卸载与恢复

| 操作 | 行为 |
|---|---|
| 卸载 | 删 `assets/plugins/<dir>`（用户目录那份）+ 删 kv；内置插件留下墓碑，记录"用户主动删的" |
| 恢复内置 | 从本体自带的 `assets/plugins/<dir>` 重新复制，墓碑清掉 |
| 升级 | 内置包版本 > 用户目录版本时自动替换 |

墓碑（`removed_builtins`）存在的意义：用户删了内置插件，下次启动不能又冒出来。

### 7.7 为什么要改成 Release 分发

最早的设想是「做个扩展 crate，本体 `cargo add` 它」。但这与"插件单独一个仓库、
独立发版"直接冲突：

| 方案 | 问题 |
|---|---|
| Cargo 依赖（git 或 path） | 升级插件要改本体 `Cargo.toml` + 重新编译本体；用户拿到的是"重新编译过的 exe"，不是插件升级 |
| `[patch]` 覆盖 | 同上，且 `plugin_host` 的类型要跨 crate 统一，插件仓库得反向依赖本体 crate（循环依赖） |
| 动态库（cdylib） | Rust 没有稳定 ABI；Windows 上还要处理加载路径、符号解析 |

只剩两条真正可用的路：**WASM 运行时加载**与**子进程**。本项目选前者（沙箱内、
无 ABI 问题、跨平台），于是"插件的交付物"必然是一个字节流，而"本体仓库只留锁文件、
构建时从 Release 拉"成为自然结论。这也顺带解决了三个老问题：

1. 本体仓库不再因为插件源码而变胖、变慢（CI 不再编译插件）；
2. 插件可以独立迭代、独立 issue/PR，不必碰本体；
3. 插件崩了只死一次调用，本体继续跑（进程内 WASM 的 fuel/超时隔离）。

代价是本体的"内置插件"变成"构建时下载的外来包"，因此必须有两层校验
（zip 的 sha256 + 包内 `check.sha256`），并且在没网时能以"零插件"启动。

---

## 8. 插件开发

### 8.1 仓库结构（每插件一仓库）

```
rcgui-plugin-hello/
  Cargo.toml            # crate-type = ["cdylib"]? 不需要：WASI 命令即 bin
  src/main.rs           # 全部逻辑；stdin/stdout 一行 JSON
  config.json           # 元数据 + plugin_setting
  scripts/
    pack.ps1            # 构建 + 生成 check.sha256 + 打 zip
  .github/workflows/release.yml
  README.md             # 必须链接本体仓库
  LICENSE
```

`Cargo.toml`：

```toml
[package]
name = "rcgui-plugin-hello"
version = "0.1.0"
edition = "2021"

[lib]                     # 若把逻辑写成 lib 便于测试
name = "hello_plugin"

[[bin]]
name = "hello"
path = "src/main.rs"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[profile.release]
opt-level = "s"
lto = true
strip = true
panic = "abort"
```

> 产物名必须与 `config.json.entry` 对应。上面的 `[[bin]] name = "hello"` 产出
> `hello.wasm`，那 `entry` 就写 `"hello.wasm"`；若想统一，把 bin 名改成
> `main` 并让 `entry` 用默认的 `main.wasm`。

### 8.2 打包脚本

```powershell
# scripts/pack.ps1
$ErrorActionPreference = 'Stop'
cargo build --release --target wasm32-wasip1
$cfg  = Get-Content config.json -Raw | ConvertFrom-Json
$entry = if ($cfg.entry) { $cfg.entry } else { 'main.wasm' }
$out  = "dist/$($cfg.id)-$($cfg.version)"
New-Item -ItemType Directory -Force -Path $out | Out-Null
Copy-Item config.json $out
Copy-Item "target/wasm32-wasip1/release/$entry" "$out/$entry"
$files = 'config.json', $entry
$files | ForEach-Object {
  '{0}  {1}' -f (Get-FileHash "$out/$_" -Algorithm SHA256).Hash.ToLower(), $_
} | Set-Content "$out/check.sha256" -Encoding ascii -NoNewline
Compress-Archive -Path "$out/*" -DestinationPath "dist/$($cfg.id)-$($cfg.version).zip" -Force
# 打印 zip 的 sha256，填进本体 plugins/manifest.json
(Get-FileHash "dist/$($cfg.id)-$($cfg.version).zip" -Algorithm SHA256).Hash.ToLower()
```

### 8.3 WASM 约束

| 约束 | 原因 |
|---|---|
| 目标 `wasm32-wasip1` | 宿主用 wasmtime + WASI preview1 |
| 不要用 `wasm-bindgen` | 那是浏览器/JS 的 ABI，宿主不提供 |
| 不要用线程、`std::process`、直接 `std::net` | 一律走能力（`proc.*` / `http.*`） |
| 不要假设 cwd | 用 `fs.*` 能力拿绝对路径 |
| 体积尽量 < 3 MB | 会被打进安装包 |
| 不要写 `println!` 打印调试信息以外的内容 | stdout 是协议通道，多一行就解析失败。调试用 `eprintln!`（走 stderr，宿主记日志） |

### 8.4 骨架

```rust
use serde_json::{json, Value};
use std::io::{Read, Write};

fn main() {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).unwrap();
    let req: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let id = req["id"].as_u64().unwrap_or(0);
    let method = req["method"].as_str().unwrap_or("");
    let params = &req["params"];

    let out = if method == "manifest" {
        json!({ "id": id, "manifest": {
            "id": "hello", "name": "示例", "version": "0.1.0", "description": "最小可用插件"
        }})
    } else {
        let action = params["action"].as_str().unwrap_or("");
        let provided = &params["provided"];
        match action {
            "get" if provided.is_null() => json!({
                "id": id, "state": { "action": "get" },
                "need": [{ "cap": "kv.get_all", "key": "kv", "params": {} }]
            }),
            "save" => {
                let form = &params["form"];
                json!({ "id": id,
                    "ops": [{ "op": "kv.set", "key": "cfg", "value": form }],
                    "result": { "status": 200, "body": {
                        "status": "已保存", "lamp": "ok", "toast": { "level": "info", "message": "已保存" }
                    }}
                })
            }
            _ => json!({ "id": id, "result": { "status": 200, "body": {
                "values": provided.get("kv").cloned().unwrap_or(json!({})),
                "status": "就绪", "lamp": "ok"
            }}}),
        }
    };
    let _ = std::io::stdout().write_all(out.to_string().as_bytes());
}
```

### 8.5 调试

| 手段 | 用法 |
|---|---|
| 看插件报错 | 界面 toast + `插件 <id> 出错: …`；本体日志有完整 JSON |
| 本地直跑 | `echo '{"id":1,"method":"manifest"}' \| wasmtime run main.wasm` |
| 看 stdout 污染 | 解析失败时本体把原始 stdout 贴进错误信息 |
| 单测 | 把 `handle(req) -> Value` 抽成纯函数，在插件仓库里用普通 `cargo test` 测 |

---

## 9. 安全边界

| 边界 | 实现 |
|---|---|
| 插件无法直接访问系统 | WASI 不预开目录/网络；一切经由 `need` → 宿主实现 |
| 能力白名单 | `capabilities` 未声明的组，宿主在分发前拒绝 |
| 资源上限 | fuel、栈、轮数、超时（§5.4） |
| 包完整性 | `check.sha256` 校验入口文件；发布包另有 zip 级 sha256（§7.2） |
| 隐私 | `fs.*` / `proc.*` / `http.*` 都是高危能力，`capabilities` 里应**最小化声明**；插件说明里要写清楚会碰什么 |
| 提权 | 本体不提权运行；`env.info.elevated` 仅供插件判断，插件不得尝试绕过 |

`proc.run` 会以当前用户身份执行任意程序——这是插件能做的事，所以**插件来源
必须是用户信任的仓库**。本体只负责"事前声明 + 事后可卸载"，不负责沙箱化插件
调用的外部程序（这超出 WASM 沙箱的范围）。

---

## 10. 迁移清单（从旧架构到本架构）

旧的镜像图床插件（`imagehost`）曾是本体的特例：`crates/app/src/bed.rs`
（1117 行业务逻辑）、`main.slint:4281-4570`（287 行专属界面）、
`http.rs` 里的 `/upload` 专线、`Cmd::Ih*` 一串命令。这些都是反例。

迁移一个旧内置插件到独立仓库，按顺序做：

1. **搬源码**：`plugins/<id>/` → 新仓库 `rcgui-plugin-<name>/`；
2. **写 `config.json`**：把原来写死在 Slint 里的界面翻译成 `plugin_setting.controls`；
3. **业务逻辑搬进 wasm**：原来 `bed.rs` 那种"本体直接算"的部分，改成
   `need` + `ops`（rclone 操作用 `rclone.call`，上传用 `rclone.upload_file`，
   短链 POST 用 `http.request`）;
4. **补能力**：如果现有能力不够，在**本体**的 `caps.rs` + `cap_of_op` 里加
   **通用** op（例如 `http.request`、`ui.copy`），而不是给某个插件开专线；
5. **删干净**：`bed.rs`、Slint 里 `if p.id == "<id>"` 的整段、`main.rs` 的
   `Cmd::Ih*` 与专线 URL、`build.rs` 的环境变量、CI 里的插件编译步骤；
6. **加清单**：`plugins/manifest.json` 一条记录；
7. **验收**：`cargo build` 通过；插件页完全由声明式面板渲染；卸载插件后本体
   仍能正常启动；全局搜不到插件 id 字符串（除 `manifest.json`）。

---

## 11. 参考实现

| 插件 | 仓库 | 说明 |
|---|---|---|
| 一键 rclone | <https://github.com/h897105167/rcgui-plugin-oneclick>（id `onerclone`） | 完整体验：12 个动作、环境自检、下载安装、远程创建、rclone 调用 |
| 图床 | <https://github.com/h897105167/rcgui-plugin-imagehost>（id `imagehost`） | 上传 → 直链 → 短链；rclone 上传 + HTTP 短链 + 文件选择 |

两个插件仓库的 README 都引用本体：
<https://github.com/h897105167/rclone_gui_windows>

它们的 `scripts/pack.{ps1,sh}` 与 `.github/workflows/release.yml` 就是"每插件一仓库"
的样板：打 tag `v<version>` → CI 校验 tag 与 `config.json.version` 一致 → 构建
`wasm32-wasip1` → 打包 `dist/<id>-<version>.zip` → `gh release create`。
本体这边只改 `plugins/manifest.json` 的 `tag` 与 `sha256`（见 §7.2 的 ⚠️）。

---

## 12. 变更记录

| 契约版本 | 变更 |
|---|---|
| 1 | 初版：目录包 + `config.json`/`check.sha256`、declarative panel、`need`/`provided` 协商、8 个能力组、`plugins/manifest.json` 构建期拉取 |
| 1.1 | 契约版本仍为 1，以下为同一版本内的兼容扩展：新增通用能力 `http.request`（短链/任意 HTTP）、`ui.copy`（剪贴板）、`rclone.cat`（读远程小文件）、`fs.read_b64`；`imagehost` 插件移出本体、改由 Release 分发；本体删除 `bed.rs` 与图床专属界面 |
| 1.2 | 补齐文档缺口：`config.json.http_entry` 与 §5.7 内嵌 HTTP 入口（请求快照 + 响应约定）、§7.7 为什么用 Release 分发；本体删掉最后一处 `id == "imagehost"` 特判与已废弃的 `PluginsG.loaded` |
