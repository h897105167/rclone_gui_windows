# 性能预算与测量

这份文档回答三个问题：量什么、怎么量、什么样的数字可以写进 README 和发布说明。

一句话原则：**这里不写推测的数字。** 任何一个出现在文档里的性能数字，都必须来自 `scripts/measure-perf.ps1` 在某一台具体机器上的一次真实运行，并连同机器、版本、构建方式一起记录。没有量到的项就写"未测量"，不要填一个看起来合理的值。

---

## 1. 量什么

| 指标 | 定义 | 为什么关心 |
| --- | --- | --- |
| 启动时间 `startup_ms` | 从 `Start-Process` 到主窗口出现（或 `-HealthUrl` 返回 2xx）之间的耗时 | 用户点开图标到能用之间的等待 |
| 工作集 `working_set_mib` | 稳定后的 `Process.WorkingSet64` | 用户任务管理器里看到的"内存占用" |
| 私有内存 `private_mib` | 稳定后的 `Process.PrivateMemorySize64` | 进程独占的已提交虚拟内存，可能驻留也可能换出，不等于物理内存 |
| 峰值工作集 `peak_working_mib` | `Process.PeakWorkingSet64` | 大目录列表、大配置文件时的瞬时高点 |
| 可执行文件 `exe_bytes` | `target\release\rclone-gui.exe` 的字节数 | 下载体积与解压后体积 |
| 解压体积 `package_bytes` | portable 目录里所有文件字节数之和（含内核与插件） | 安装占用；压缩下载大小需另读 zip 文件字节数 |

工作集与私有内存的区别：工作集包含可被系统回收的页面，私有内存是进程独占的部分。两者都要报，只报一个容易造成"看起来变小了"的错觉。

## 2. 怎么量

```powershell
# 默认：target\release\rclone-gui.exe，采样 3 次，等主窗口
.\scripts\measure-perf.ps1

# 指定产物、采样 5 次、稳定 5 秒后读内存
.\scripts\measure-perf.ps1 -Exe .\target\release\rclone-gui.exe -Samples 5 -SettleMs 5000

# 同时给出 portable 包体大小
.\scripts\measure-perf.ps1 -PackageDir .\dist\RCGUI-0.3.0-windows-x64-portable

# 机器可读，方便贴进发布说明或做前后对比
.\scripts\measure-perf.ps1 -Json | Tee-Object -FilePath perf-0.3.0.json
```

脚本的行为，按发生顺序：

1. 解析仓库根目录、检查 `-Exe` 是否存在，算它的字节数和 SHA-256。
2. 重复 `-Samples` 次：给这一次采样建一个独立数据目录，改掉子进程的 `APPDATA`／`LOCALAPPDATA`／
   `TEMP`／`TMP`／`RCGUI_DATA_DIR`／`RCLONE_CONFIG`，然后**先开表再启动**——进程创建本身也算
   启动开销，`Start-Process` 之后再开表会把这段从数字里剪掉。等它就绪，先确认 `RCGUI_DATA_DIR`
   指向的目录里真的出现了程序自己写下的非空文件（等证据的时间不计入 settling），再从就绪时刻
   起走满 `-SettleMs` 才读三项内存；最后在 `finally` 里按 PID 树只结束自己拉起来的那些进程。
3. 汇总成 JSON 或打印成表格。

参数含义：

| 参数 | 默认 | 说明 |
| --- | --- | --- |
| `-Exe` | `target\release\rclone-gui.exe` | 被测产物；相对路径按仓库根算 |
| `-Samples` | `3` | 采样次数；至少 1 |
| `-StartupTimeoutMs` | `30000` | 单次等待就绪的上限 |
| `-SettleMs` | `3000` | 就绪后必须走满的一段等待，少一毫秒都不读内存：早读量到的是启动瞬间的分配尖峰，不是稳定占用 |
| `-HealthUrl` | 空 | 给了就用 HTTP 2xx 作为就绪信号，例如 `http://127.0.0.1:5273/api/health`；不给就看主窗口句柄 |
| `-PackageDir` | 空 | 给了就统计该目录总字节数，作为包体 |
| `-Json` | 关 | 输出 JSON |
| `-RepoRoot` | 自动 | 仓库根目录；自动探测失败时手动指定 |
| `-DataRoot` | 空 | 每次采样的独立数据目录放在哪；空表示在本轮临时目录里新建一个（跑完自动删） |
| `-KeepData` | 关 | 保留独立数据目录，方便事后看里面到底写了什么 |

退出码：

| 退出码 | 含义 |
| --- | --- |
| `0` | 至少一次采样"启动时间 + 内存"都读到了，数字可用 |
| `1` | 参数不合法或找不到被测文件；没有启动任何进程 |
| `2` | 一次可用采样都没有；输出的数字全是 `null`，不要引用 |

## 2.1 为什么每次采样都要换数据目录

程序的数据目录有三档优先级（`crates\app\src\config.rs` 的 `data_dir()`）：`RCGUI_DATA_DIR` 有值就用它，
不读 `%APPDATA%`、也不做旧目录迁移；没有这个变量时，exe 旁边有 `portable` 标记就写 `exe目录\data`；
两者都没有才写 `%APPDATA%\RCGUI`。环境变量是进程从父进程继承的，所以这套优先级决定了脚本能把数据
目录钉在哪。

在同一台机器上直接量，会有两个坑：

- 机器上已经开着一个旧版 RCGUI 时，新进程要么连到它、要么和它抢同一个默认端口 5273。
  端口抢不到不会让程序退出（主窗口照常出现，HTTP 服务只是没起来），于是"启动成功"却测的不是
  一个干净实例。
- 上一次采样留下的配置、缓存、插件目录会被下一次复用，第二次启动自然更快，中位数就被拉低。

因此脚本对每一次采样新建一个 `sampleNN\`，把 `APPDATA`、`LOCALAPPDATA`、`TEMP`、`TMP` 指进去，
并把 `RCGUI_DATA_DIR` 指向 `sampleNN\data\`。

`RCLONE_CONFIG` 也得一起指进去。这台机器上早就设过这个变量的话，`rclone.exe` 会照它去读真实
那份 `rclone.conf`——数据目录隔离就成了摆设，测量还会顺手改到用户的真实配置。脚本把它改成
`sampleNN\data\rclone.conf`，跑完连同其它变量一起复原。

**证据口径：只有程序自己写下的文件才算数。** 启动后脚本盯着 `sampleNN\data\`，里面出现
由被测进程写下的非空文件（`config.json`、日志、缓存之类），才认为这次确实用了这套隔离目录。
空文件不算——打开又没写内容的占位文件证明不了它读过这份配置。脚本自己预置的文件同样不算：
`Get-IsolationEvidence` 带排除名单，将来若要预放模板（比如一份 `rclone.conf`），那个文件不能
冒充"程序写过"。

不能用别的什么当证据：

- `sampleNN\data\` 这个**目录**本身不算：它是脚本为了喂 `RCGUI_DATA_DIR` 自己建的，永远存在。
  拿"目录在不在"当判据，任何一次启动都会通过，隔离就等于没做。
- `LOCALAPPDATA`、`TEMP` 下的东西不算：.NET／WinForms 运行时自己会建
  `LOCALAPPDATA\Microsoft\Windows\Caches`，那跟被测程序有没有读数据目录无关，等于次次通过。
- `APPDATA` 下的子项**不再**当隔离证据，只作兼容统计（`appdata_children`）：认 `RCGUI_DATA_DIR`
  的程序不会往那里写，所以这个数大于零恰好说明跑的是没认这个变量的旧版本。

看不到文件就把这次采样判为失败，`startup_ms` 置 `null` 并写明原因；如果同时看到 `APPDATA` 里被写了
东西，原因里会点明"这个 exe 没认 RCGUI_DATA_DIR（旧版本）"。宁可少一个样本，也不把一个
"可能连的是别人"的启动时间混进中位数。

清理同样只对自己动手：按启动时的父进程关系算出这棵 PID 树（程序本体 + 它拉起的受管
`rclone.exe`），先杀子进程再杀父进程，杀完复查一遍有没有漏网的并写进 `error`。
自己建的临时根目录在跑完后删除，删之前会核对"绝对路径 + 在 `%TEMP%` 之下 + 目录名以
`rcgui-perf-` 开头"三条；用 `-DataRoot` 指定的目录不删，`-KeepData` 时也不删。

## 3. 怎么读结果

JSON 里和计数有关的三行必须能对上：`samples_ok + samples_failed = samples_requested`。

- `samples_ready`：等到了就绪、拿到了启动时间的次数。
- `samples_ok`：启动时间和内存**都**读到的次数，只有这些数字参与内存统计。
- `samples_failed`：`samples_requested - samples_ok`，包含"测到一半进程就退了"的情况；这种采样在 `runs[]` 里有一条 `error` 说明原因。
- `samples_isolated`：确实在 `RCGUI_DATA_DIR` 目录里看到程序写下文件的次数。它应当等于 `samples_ready`；
  比它小就说明有采样没确认成干净实例，那些样本的 `startup_ms` 是 `null`。
  每条 `runs[]` 的 `data_dir_seen` 写着看到的是哪些文件（相对路径），`data_files` 是文件个数，
  用来回溯。
- `isolation`：口径说明本身——`data_dir_override`（钉住数据目录用的是 `RCGUI_DATA_DIR`）、
  `config_override`（`RCLONE_CONFIG` 被指到哪个隔离路径）、`evidence`（什么算证据：该目录下由
  被测程序写下的非空文件，脚本预置的不算）、`appdata_compat`（`APPDATA` 子项数只作兼容统计）、
  `samples_with_files`（真正看到文件的采样数）。
- `timing`：这轮数字的口径——`startup_includes_process_creation`（启动时间含进程创建）、
  `memory_read_after_settle_ms`（内存是在就绪之后等满了多久才读的）。
- 每条 `runs[]` 的 `ready_at_utc`／`settled_at_utc`／`settle_ms`：就绪时刻、读内存的时刻、中间要求的
  间隔。想核对"到底等满了没有"，把两个时间戳相减，应当不小于 `settle_ms`。
- `data_root` / `data_root_owned` / `data_root_removed`：这轮用的隔离根目录、是不是脚本自己建的、
  最后有没有删掉。`data_root_owned: false` 说明用的是 `-DataRoot` 给的目录，脚本不会去删它。

所以出现 `samples_ready: 1` 而 `samples_ok: 0` 是正常状态：那一次确实测到了启动耗时，但进程在读内存前退出，于是它只贡献启动时间的中位数，不贡献内存数字。**不要**为了让表格好看把 `null` 改成 `0`。

中位数取法：排序后取 `floor((n-1)/2)` 位置，也就是偶数个样本时取偏小的那个，而不是两个数的平均。这样报告里出现的每个中位数都是某一次真实采样，能被复现。

## 4. 预算

预算是"超过就要停下来解释"的门槛，不是已经达到的成绩。首次正式记录之前，预算值标为待定，因为凭空写一个数字就等于编造。

| 项目 | 预算 | 状态 |
| --- | --- | --- |
| 启动时间（冷启动，参考机） | 待首次实测后确定 | 未测量 |
| 工作集（空配置，闲置 10 秒） | 待首次实测后确定 | 未测量 |
| 私有内存（同上） | 待首次实测后确定 | 未测量 |
| release exe | 待首次实测后确定 | 未测量；0.2.1 时代的旧产物为 47,661,568 字节，仅作参照，不是 0.3.0 的数字 |
| portable 包体 | 待首次实测后确定 | 未测量 |

定预算的规则：同一个脚本、同一台机器、同一个构建模式（release）、同样的 `-Samples`，连续两次测量取较小的中位数作为基线，然后把基线写进上表，再把"超过基线 10% 需要在本文件里补一条原因"写成回归规则。跨机器、跨 debug/release 的比较不算数。

## 5. 已知限制

- **Win32 别名程序测不到。** Windows 11 的 `notepad.exe`、`calc.exe` 是应用执行别名：`Start-Process -PassThru` 拿到的进程会立刻以退出码 0 结束，真正的窗口属于另一个进程。脚本会如实报 `进程在就绪前退出（退出码 0）`，不要据此判断脚本坏了。
- **就绪信号是"主窗口句柄非零"。** 托盘常驻、启动即最小化的构建可能长时间没有主窗口，此时请用 `-HealthUrl`，或者把 `-StartupTimeoutMs` 放宽并在文档里说明用的是哪种信号。
- **单机单次波动大。** 杀毒软件扫描、磁盘缓存冷热、后台更新都会影响首次数值，所以默认采样 3 次并报中位数与最大值。少于 3 次的数字不要写进发布说明。
- **CI 上量出来的数不作为发布数字。** 共享 runner 的 CPU 与磁盘不可控，只能用来发现"比上周慢了一倍"这类量级问题。
- **不测量 rclone 内核。** 本脚本只量 RCGUI 自己这个进程；受管内核的内存属于另一个进程，要单独量，并且必须在文档里注明是哪个内核版本。

## 6. 工具自身的验证记录

脚本本身也在被测。以下记录只证明脚本行为和退出码是对的，**不是** RCGUI 的性能数字：

| 场景 | 命令要点 | 结果 |
| --- | --- | --- |
| 语法检查 | `[Parser]::ParseFile` | 通过（1786 tokens） |
| 找不到产物 | `-Exe` 指向不存在的文件 | 退出码 1，提示先构建 |
| 参数不合法 | `-Samples 0` | 退出码 1 |
| 全部采样失败 | 被测程序在就绪前退出 | 退出码 2，各统计值为 `null`，连续 12 次结果一致 |
| 成功路径 | `-Exe C:\Windows\System32\mstsc.exe -Samples 2`（替身，不是 RCGUI） | 退出码 0，`samples_ok=2`，启动 187.2/202.0 ms，工作集 34.1/34.2 MiB，私有 5.5 MiB，测量后无残留进程 |

那张表里的 187.2 ms 与 34.1 MiB 属于 `mstsc.exe`，与 RCGUI 无关，放在这里只为证明脚本能读出真实数字。

## 7. 把数字写进发布说明

发布说明里的一段性能记录至少包含：

- 版本与 git 提交、构建方式（release profile 见根 `Cargo.toml`）。
- 机器：CPU 型号、内存容量、Windows 版本、是否有实时防护在跑。
- 被测文件的字节数与 SHA-256（脚本会输出）。
- 采样次数、`-SettleMs`、就绪信号（窗口还是 HTTP）。
- 中位数与最大值；如果和上一版对比，写绝对值差与百分比，并说明比较的两台机器是否同一台。

缺任何一项，就只写"本次未测量性能"，不要留一个孤零零的数字让别人去猜它是怎么来的。
