# UI 交互实测验证（2026-10-19）

> 上一轮交接文档写着「UI 交互仍无法自动化验证 —— 本机 Slint 内容区点击送不进去」。
> 本轮把这条打通了：**用 Win32 SendInput 驱动真实窗口**，全部验证项都拿到了可复现证据。
> 本文件记录方法、结果、以及由此发现的缺陷。

## 一、为什么上一轮送不进去：三个根因

都不是「Slint 收不到点击」，而是坐标与前台没对：

1. **DPI 未感知**。会话默认 DPI 168（175%）。DPI-unaware 进程拿到的坐标是虚拟化的
   （2194×1234 而不是 3840×2160），任何按截图算出的坐标都会整体缩放错位。
   → `SetProcessDpiAwarenessContext(-4)` / `SetThreadDpiAwarenessContext(-4)`。
2. **双显示器 + 虚拟桌面原点为负**。本机虚拟桌面原点 `(-1440,-402)`、尺寸 `5280×2562`。
   用主屏 `3840×2160` 归一化绝对坐标，点击会落到另一块屏幕上
   （实测：目标 `(189,220)` 变成光标 `(-1181,-141)`）。
   → 要么按虚拟桌面归一化，要么直接用 `SetCursorPos`（物理坐标，不受归一化影响）。
3. **前台锁**。`SetForegroundWindow` 会被系统拒绝，输入于是打给了别人。
   → 先补一次 Alt 键（让自己成为「最后输入者」），再 `AttachThreadInput` 到当前前台线程，
   然后 `SetForegroundWindow`；**并且在发送任何输入前断言目标窗口确实是前台**。

## 二、可复用的做法

仓库外脚本 `ui_probe/UiProbe.ps1`（PowerShell + Add-Type 的 C#，无第三方依赖）：

| 能力 | 说明 |
|---|---|
| `Find-AppWindow` | 按进程名 + 窗口类枚举真实 HWND（含隐藏窗口） |
| `Save-WindowShot` | 先置前再按窗口矩形截屏；`Stats` 给出非黑像素比例，防止拿到全黑图 |
| `Find-AccentButtons` | 按强调色聚类出按钮矩形，**不靠肉眼估坐标** |
| `Set-UiField` | 点击 → Ctrl+A → Delete → 输入，用于覆盖已有值 |
| `[Ui]::TrayRect` | `Shell_NotifyIconGetRect` 拿托盘图标真实屏幕矩形 |
| `Compare-Shot` | 两帧差异百分比，作为「界面确实变了」的客观证据 |

几条踩坑后的硬性做法：

- **只用截图做判断前，先把坐标量准**：直接对 PNG 做像素扫描（文字行、边框颜色变化），
  不要用「显示尺寸 × 缩放系数」心算 —— 本轮因此点错按钮三次。
- **点击前断言前台**，否则可能点到别的窗口（本轮曾把窗口点成最大化）。
- 托盘图标常常在**溢出区**里：`Shell_NotifyIconGetRect` 在溢出面板关闭时返回的是
  折叠按钮附近的矩形，先点 `^` 打开溢出面板，再取一次矩形才是真位置。
- 提示类消息只显示 2.6 秒，抓不到就**去仪表盘的「历史记录」取证** —— 那里有全文。

## 三、验证结果（全部通过）

| 项目 | 证据 |
|---|---|
| 左侧导航切换 | 点击后页面切换、截图差异 35%+ |
| 连接 / 握手 | 真实 401 → Basic 认证链路；状态变「已连接」，显示 rclone 版本与配置路径 |
| 远程页代理列下拉 | 选中后 rclone 侧出现 `override.http_proxy = http://127.0.0.1:27723` |
| 删除确认框（远程） | 弹出位置正确、`取消` 不删、`删除` 真删（`config/listremotes` 少一条） |
| 底部状态栏三项 | 传输→传输页、挂载→挂载页、错误→底部错误面板 |
| 错误面板 `关闭` / `清空` | 关闭后精确回到原页面（像素差 0%）；清空后计数归零 |
| 仪表盘历史记录 `清空` | 变为「暂无记录」 |
| 托盘：单击 | 窗口保持最小化（符合「单击不动作」） |
| 托盘：真实双击 | 最小化状态 → 恢复并置前（走真实 `Shell_NotifyIcon` → `WM_USER_TRAYICON` 链路） |
| 托盘：右键菜单 | 弹出原生 `#32768` 菜单「主界面/退出」；点「主界面」恢复窗口 |
| 新建代理表单 | 名称/地址/端口输入、保存校验（空名称、空地址都有明确提示） |

未执行：托盘菜单「退出」。它会卸载挂载并结束程序，属于会改变用户运行态的破坏性操作，
不在本轮授权范围内。

## 四、由此发现的缺陷（本轮已修）

1. **远程列表只渲染第一行**。标题写着「远程存储（4）」，列表里只画出 1 条，滚轮也滚不动。
   原因：`Card` 内部 `VerticalLayout` 的对齐是 `start`（只有 `fill: true` 才是 stretch），
   列表容器只写了 `vertical-stretch: 1`，拿不到剩余空间 → 高度塌成一行。
   修法：卡片高度与列表高度**都由行数推导**（`74px + min(360, max(40, 行数×40))`），
   不依赖布局结果。注意 `Card` 内建标题行也占 36px，漏算会让最后一行被裁。
2. **启动时代理配置表不拉取**。重启后远程行显示着代理名，下面的「代理配置（0）」却是空表，
   必须手动点刷新。修法：初始加载序列补 `Cmd::ProxyListRefresh`。
3. **确认弹窗在窗口底部会被裁掉**。`pop-ay` 只按行位置算，靠近窗口底边时按钮跑到窗口外。
   修法：按**窗口可视高度**收（不能用 `root.height` —— 整页 ScrollView 里它是内容高度），
   并把 ConfirmPop 的正文 `Text` 给定宽度，否则 `preferred-height` 按单行估算、正文换行后
   把按钮挤出卡片。

> 排除的误报：代理配置的「删除确认」不是弹层，而是页面内联的一块，滚一下就能看到按钮 ——
> 不算缺陷，但**位置偏低**，仍建议后续改成锚定式弹窗。

## 五、截不到图时的排查顺序（会话 20 实测）

症状：进程活着、`IsWindowVisible = true`、`SendMessageTimeout(WM_NULL)` 有响应、CPU/句柄都正常，
但 `CaptureWindow` 抓到的永远是桌面或别的东西，`CapturePrint`（PrintWindow）全黑。

按这个顺序排，基本一次能定位：

1. **窗口是不是从来没被 Slint 画过**。`%APPDATA%\RCGUI\config.json` 里 `silent_start: true` 时，
   `crates/app/src/main.rs` 的启动分支直接跳过 `ui.show()`，Slint 永远不出第一帧 ——
   此时 `ShowWindow(SW_SHOW/SW_RESTORE)`、`SetWindowPos(SWP_SHOWWINDOW)`、`SetForegroundWindow`
   全部"成功"但没有像素。判据：`DwmGetWindowAttribute(DWMWA_CLOAKED)` 也是 0、窗口能最小化/还原，
   就是画不出内容。
2. **本机 DSH 桌面窗口盖住了主屏**。`class = 'Tauri Window'` 的那扇窗是 always-on-top 且铺满主屏，
   在主屏上抓任何窗口都只会得到它。把被测窗口挪到副屏（本机虚拟桌面原点是负的：`-1440,-402`）。
3. **点不到 / 点了没反应**，先分清是"输入没送到"还是"命中了空白"：
   - 输入是否送到：点标题栏的最小化按钮，看窗口是否真的最小化；
   - 是否命中：拿控件几何**算**坐标，别凭截图目测 —— 例如左侧导航栏
     `main.slint:5183` 是 `width: 64px; padding-top: 8px; spacing: 2px; height: 52px/项`，
     第 7 项（插件）在 125% 缩放下 client y ≈ 494；y = 452 会落在第 6 项（传输）里。
4. **想看面板就得换个 profile 启动**（不动用户配置与运行态）：

   ```powershell
   $roaming = '<tmp>\bedprof\Roaming'
   New-Item -ItemType Directory -Force "$roaming\RCGUI" | Out-Null
   Copy-Item "$env:APPDATA\RCGUI\config.json" "$roaming\RCGUI\config.json"   # 需要真配置时
   # 把 silent_start / connect_on_start 改成 false
   $env:APPDATA = $roaming
   $env:RCGUI_SINGLETON = 'bedtest'        # crates/app/src/main.rs 读这个名字做单实例互斥
   Start-Process <release>\rclone-gui.exe -WorkingDirectory <release>
   ```

   `config.rs:233` 用 `APPDATA` 定位配置，`main.rs:2767` 用 `RCGUI_SINGLETON` 做互斥名 ——
   两件事凑齐就能与用户实例并存，且窗口会正常 `ui.show()`。
   注意 `connect_on_start: false` 时面板里「存储的远程」下拉是空的（RC 没连），
   要验远程列表就让它连真 rclone，别在隔离 profile 里连。

5. 抓图仍要有"这张图是不是真的新"的判据：比较两次截图的 `Get-FileHash`。相同就说明界面没重绘，
   别把旧帧当成"点击无效"的证据 —— 反过来也别把旧帧当成"点击生效"的证据。
