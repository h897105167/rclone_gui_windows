# 无界面诊断接口

状态：0.3.0 已有实现，尚未发布。实现位于 `crates/app/src/cli.rs`，在窗口、托盘与单实例初始化之前执行。

```powershell
.\rclone-gui.exe --version
.\rclone-gui.exe --diagnostics --json
.\rclone-gui.exe --list-plugins --json
.\rclone-gui.exe --validate-config .\rcgui.conf --json
.\rclone-gui.exe --export-accessibility-tree .\diagnostics\ui.json
```

这些命令不打开 GUI，不启动长期运行的 rclone 内核，不连接云端。诊断和插件列表会探测本机内核版本，因此可能短暂执行选定的 `rclone.exe version`。不带参数正常打开窗口；`--silent` 是单独的 GUI 启动标志。未知参数返回错误，不能静默打开窗口。

## 参数和退出码

参数大小写敏感；路径用空格分隔，含空格时加引号，不支持 `--validate-config=...` 写法。建议每次只调用一条命令；混用时按帮助、版本、诊断、插件列表、配置校验、语义快照的顺序取第一条。`--json` 用于诊断、插件列表和配置校验；`--version --json` 仍输出文本 `rcgui 0.3.0`。

| 退出码 | 含义 |
| --- | --- |
| 0 | 命令执行成功；诊断仍可能有警告，需读取 `warnings` |
| 1 | 运行错误，例如无法生成诊断或写入快照 |
| 2 | 参数错误，包括未知标志、缺少参数、校验目标文件不存在 |
| 3 | 配置文件可读取，但不能通过导入预检 |

stdout 为结果，stderr 为错误。机器读取 JSON 时不要根据中文文案判断成功与否。

## 诊断

`--diagnostics --json` 返回对象，主要字段如下。以当前构建的真实输出为准。

| 字段 | 内容 |
| --- | --- |
| `schema`, `tool`, `version`, `build`, `os`, `arch` | 报告版本与构建身份 |
| `scope`, `runtime` | CLI 为 `offline`、`false`；应用内诊断为 `in-app`、`true` |
| `data_dir`, `config_path` | 实际位置，用户主目录前缀替换为 `%USERPROFILE%` |
| `config` | 配置可读性、内核模式、启用插件数量、同步任务数量与语言 |
| `core` | 定位到的可执行文件、来源、版本探测结果与错误 |
| `plugins` | `dir`、已安装插件的 `items`、坏包的 `errors` |
| `features` | 编译功能，包括真实 UIA 支持与静态语义导出的区别 |
| `cache`, `lightweight`, `current_mode` | 缓存与轻量模式信息；离线进程看不到的运行态为 `null`，并带原因 |
| `sync`, `logs`, `warnings` | 同步配置摘要、日志文件名和大小、需要处理的问题 |

CLI 不查询正在运行的另一个 GUI 进程。不能将 `core.version` 解释为“该内核正在运行”，也不能将 `lightweight.active: null` 解释为“轻量模式未开启”。同步任务启用时，离线报告的运行状态为 `unknown`。

不输出 RC 密码、OAuth token 或配置文件正文。路径只遮蔽已知用户主目录前缀；自定义目录和日志文件名仍可能包含个人信息，公开报告前应检查。

## 插件列表

`--list-plugins --json` 返回 `{ "dir": ..., "items": [...], "errors": [...] }`。每个有效项目包含 `id`、`version`、`enabled`、`verified`；损坏的插件在 `errors` 中报告。文本模式每行依次为 id、版本、启用状态与校验状态。

列的是本地已安装包，不是商店索引；不下载安装或更新插件。执行成功不表示 `errors` 必然为空。

## 导入预检

`--validate-config <文件>` 复用 GUI 的 `rcgui.conf` 导入预检，不是任意 `config.json` 的通用校验器。它读取当前本地配置用于比较，不应用导入，不修改启动项，不执行包内脚本。

JSON 成功结果包含 `schema`、`valid`、`file`、`summary`、`plugins`、`sync_tasks`、`changes`；失败包含 `valid: false` 和 `error`。插件包体不在配置文件中，缺少的插件需另行安装。

## 语义结构导出与真实 UIA

`--export-accessibility-tree <文件>` 创建父目录并写入 JSON。文件明确标注 `kind: "semantic-snapshot"`、`runtime: false`，列出页面、设置标签和主要动作。

这是代码声明的语义摘要，**不是当前窗口的 UIA 树**，不包含实际焦点、坐标、插件运行值或所有动态控件。它可用于无 GUI 的结构检查，不能代替读屏验收。

正式构建启用了 Slint accessibility，运行中的窗口通过 AccessKit 暴露 Windows UIA。真实控件名称、角色、键盘焦点和密码遮蔽需在运行窗口上验证；系统读屏体验还应使用 Narrator 或 Accessibility Insights 复核。

## 接口迭代

新增字段保持兼容；删除字段、修改类型或退出码必须在 changelog 中说明。调用者先读取 `schema`，对未知字段保持容忍。CLI 不提供无界面的远程删除、文件上传或同步任务控制命令；这些动作目前由 GUI 和 rclone RC 完成。
