# 隔离验收用的本地后端 fixture

父会话做「整 app + 真实 rclone」验收时用这几样东西，全程不碰用户自己的数据和配置：

1. `rclone.conf` —— rclone 自己的配置格式（不是 RCGUI 的）。里面只有一个 `local` 类型
   远程，指向随便一个本地目录，用来在没有云端账号的情况下跑通挂载、服务、同步。
2. `RCGUI_DATA_DIR` —— 把整个 RCGUI 数据目录搬到临时空目录，和用户真实安装彻底分开。
3. `RCLONE_CONFIG` —— 让本次启动的 rclone 只读 fixture 的 conf。父会话机器上正跑着用户
   自己的 rclone（pid 7880），不设这个变量就可能读写到用户的 `%APPDATA%\rclone\rclone.conf`。

```powershell
# 1. 干净的数据目录 + 只属于这次验收的 rclone 配置
$data = "$env:TEMP\rcgui-acceptance"
Remove-Item -Recurse -Force $data -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $data | Out-Null
$env:RCGUI_DATA_DIR = $data
$env:RCLONE_CONFIG  = (Resolve-Path .\crates\app\tests\fixtures\local-backend\rclone.conf)

# 2. 内核用已经校验过 SHA 的官方那份，放进数据目录下的 core\，程序自己会优先用它，
#    不会像 PATH 里那样指向用户正在跑的那个 rclone
New-Item -ItemType Directory -Force "$data\core" | Out-Null
Copy-Item .\target\release\core\rclone.exe "$data\core\rclone.exe"

# 3. 起真正的程序（不是预览用的 rcgui-ui-qa.exe，那个只有窗口壳、没有服务层）
Start-Process .\target\debug\rclone-gui.exe -ArgumentList "--silent"
```

要点：

- 目录**不需要**预置任何文件。RCGUI 第一次运行会自己生成 `config.json` 和
  `profiles/default.json`；预置一份手写 JSON 反而会在字段升级后变成坏档案。
- 内核查找顺序是「设置里指定的路径 → 数据目录/程序旁边的 core（新版优先）→ PATH」。
  把校验过的内核放进 `$data\core\` 就不会落到 PATH 上用户那份。
- `rclone.conf` 只在验收需要真远程时交给 rclone。环境变量 `RCLONE_CONFIG` 会被 RCGUI
  启动的子进程继承；RCGUI 自己不会假设配置在 `%APPDATA%\rclone\rclone.conf`，它读连接上
  的实例报回来的真实路径。
- 验收前删掉整个 `$data` 就能回到干净状态；同理，`Remove-Item $env:RCGUI_DATA_DIR` 之后
  再启动也等于全新安装（不会去迁移用户的老数据，override 目录永远不迁移）。
