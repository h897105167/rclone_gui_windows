# 量 RCGUI 的真实启动开销与内存占用。
#
# 这个脚本只做一件事：启动一个 RCGUI 进程、等它"可用"、读出真实数字、再把它关掉。
# 它不跑测试、不代替验收，也不产生估算值——读不到的数就是 null，不会补一个好看的数。
#
# 隔离：每次采样用一份全新的数据目录，并把这个目录通过 RCGUI_DATA_DIR 交给子进程。
# 程序那边（crates/app/src/config.rs 的 data_dir()）的优先级是：
#   RCGUI_DATA_DIR 有非空值 → 就是它，不看 portable 标记、不读 %APPDATA%、也不做旧配置迁移
#   exe 旁有 portable 标记 → exe目录\data
#   否则 → %APPDATA%\RCGUI（首次启动从 %APPDATA%\rclone-gui 复制一次）
# 所以设了 RCGUI_DATA_DIR 就等于把数据目录钉死在本次采样里，最干净。
# APPDATA / LOCALAPPDATA / TEMP 仍然一起指到临时目录：那是给不认上面那条规则的旧版本
# 兜底，不是主要手段。RCLONE_CONFIG 也要一起指进隔离目录：用户环境里如果早有这个变量，
# 子进程会直接去读那是真的 rclone.conf，隔离就漏了一个口子。
#
# 计时：秒表在 Start-Process **之前**开。进程创建（加载 exe、映射依赖）本身就是启动开销的
# 一部分，先起进程再开表等于把这段从数字里抹掉，量出来的会比实际更好看。
# 内存：就绪之后必须等满 -SettleMs 再读，读到的才是稳定值而不是启动瞬间的分配尖峰。
# 等待隔离证据的时间不占这个 settling——证据早到也照样等到满 -SettleMs。
#
# 证据口径：一次采样算不算"干净实例"，只看 RCGUI_DATA_DIR 指向的目录里有没有
# **程序自己写下的文件**（配置、日志、缓存都算，空文件不算）。目录本身是脚本建的，
# 光有目录什么都不证明；APPDATA 下的子项只作为兼容统计记进结果——认 RCGUI_DATA_DIR
# 的程序根本不会往那里写东西，拿它当通过条件就会把旧版本判成成功。
# 宽限期到了还没有文件，这次采样判失败、不记数字，并在错误里说清是哪种情况。
#
# 清理：只停自己拉起来的那棵进程树（本体 + 受管内核 rclone.exe），先停子进程再停本体；
# 收尾后复查一遍，还活着的 PID 会报出来，不会假装干净。
#
# 用法：
#   pwsh -File scripts/measure-perf.ps1                       # 默认量 target\release\rclone-gui.exe
#   pwsh -File scripts/measure-perf.ps1 -Samples 5 -Json      # 多采几次，输出 JSON
#   pwsh -File scripts/measure-perf.ps1 -Exe target\debug\rclone-gui.exe -HealthUrl http://127.0.0.1:5273/api/health
#   pwsh -File scripts/measure-perf.ps1 -PackageDir dist\rcgui-0.3.0-portable
#   pwsh -File scripts/measure-perf.ps1 -DataRoot D:\perf-data -KeepData   # 保留隔离目录便于排查
#
# 退出码：0 = 至少一次采样成功；1 = 参数或前置条件不满足；2 = 所有采样都失败。

[CmdletBinding()]
param(
    # 要量的可执行文件。相对路径按仓库根解析。
    [string]$Exe = "target\release\rclone-gui.exe",

    # 采样次数。少于 3 次的结果波动大，不建议拿来下结论。
    [int]$Samples = 3,

    # 单次启动最多等多久（毫秒）。超时算这次失败，不会静默记成 0。
    [int]$StartupTimeoutMs = 30000,

    # 判定"可用"之后再多等一会（毫秒）再读内存，避开启动瞬间的分配峰值。
    # 这是"就绪后必须走完的一段等待"，早一毫秒都不读内存。
    [int]$SettleMs = 3000,

    # 可选：拿内嵌 HTTP 的健康检查当"可用"信号，比只看主窗口更接近用户真实可用时刻。
    # 留空则用"进程建立了主窗口"作为信号。
    [string]$HealthUrl = "",

    # 可选：便携目录，用来量打包体积（zip 之前的总大小）。
    [string]$PackageDir = "",

    # 输出格式化 JSON，便于历史对比。
    [switch]$Json,

    # 仓库根。为空则从脚本位置向上找 Cargo.toml。
    [string]$RepoRoot = "",

    # 每次采样的隔离目录放在这里。留空则在 %TEMP% 下新建本次运行专用的目录。
    # 注意：给了这个参数，脚本不会替你删它（那是你的目录）。
    [string]$DataRoot = "",

    # 保留采样用的隔离目录，方便事后看进程到底写了什么。默认只删自己新建的目录。
    [switch]$KeepData
)

$ErrorActionPreference = "Stop"

function Write-Step([string]$Message) {
    Write-Host $Message -ForegroundColor Cyan
}

# 统计量只按"真读到值"的样本算：null 表示没测到，不能当 0 参与排序。
# Sort-Object 会把 null 丢掉，导致"有样本但数组为空"，所以这里显式挡掉空数组再取索引。
function Get-Median {
    param([object[]]$Values)
    $v = @($Values | Where-Object { $null -ne $_ } | Sort-Object)
    if ($v.Count -eq 0) { return $null }
    # 偶数个取偏小的那个中位数，规则写在 docs/PERFORMANCE.md，避免与他人数字对不上。
    return $v[[int][math]::Floor(($v.Count - 1) / 2)]
}

function Get-Extreme {
    param([object[]]$Values, [ValidateSet("min", "max")][string]$Kind)
    $v = @($Values | Where-Object { $null -ne $_ })
    if ($v.Count -eq 0) { return $null }
    if ($Kind -eq "min") { return ($v | Measure-Object -Minimum).Minimum }
    return ($v | Measure-Object -Maximum).Maximum
}

function Resolve-RepoRoot([string]$Explicit) {
    if ($Explicit) {
        if (-not (Test-Path -LiteralPath $Explicit)) { throw "RepoRoot 不存在：$Explicit" }
        return (Resolve-Path -LiteralPath $Explicit).Path
    }
    # $PSScriptRoot 在 dot-source / 脚本块调用时可能为空，退化到命令路径，再退化到当前目录。
    $start = $PSScriptRoot
    if (-not $start) { $start = Split-Path -Parent $MyInvocation.MyCommand.Path }
    if (-not $start) { $start = (Get-Location).Path }
    $probe = $start
    for ($i = 0; $i -lt 5; $i++) {
        if (Test-Path -LiteralPath (Join-Path $probe "Cargo.toml")) { return $probe }
        $parent = Split-Path -Parent $probe
        if (-not $parent -or $parent -eq $probe) { break }
        $probe = $parent
    }
    throw "找不到仓库根（向上 5 层都没看到 Cargo.toml）。用 -RepoRoot 指定。"
}

function Resolve-ExePath([string]$Root, [string]$Candidate) {
    if ([System.IO.Path]::IsPathRooted($Candidate)) { return $Candidate }
    return (Join-Path $Root $Candidate)
}

function Get-DirBytes([string]$Path) {
    # 目录不存在返回 $null，不返回 0——0 会被误读成"打包体积为零"。
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    $sum = (Get-ChildItem -LiteralPath $Path -Recurse -File -Force | Measure-Object -Property Length -Sum).Sum
    if ($null -eq $sum) { return 0 }
    return [int64]$sum
}

function Wait-UntilReady {
    # 返回 "窗口" / "http"（命中的信号），超时返回 $null。
    param(
        [System.Diagnostics.Process]$Process,
        [string]$Url,
        [int]$TimeoutMs
    )
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    while ($watch.ElapsedMilliseconds -lt $TimeoutMs) {
        if ($Process.HasExited) { return "exited" }
        if ($Url) {
            try {
                $resp = Invoke-WebRequest -Uri $Url -UseBasicParsing -TimeoutSec 2
                if ($resp.StatusCode -ge 200 -and $resp.StatusCode -lt 300) { return "http" }
            } catch {
                # 连不上就是还没起来，继续等；真正的错误在超时后统一报。
            }
        } else {
            $Process.Refresh()
            if ($Process.MainWindowHandle -ne [IntPtr]::Zero) { return "window" }
        }
        Start-Sleep -Milliseconds 50
    }
    return $null
}

function New-SampleDirs {
    # 给一次采样建一套空目录，返回各条路径。
    # 每次都用新的：上一轮留下的配置、缓存、日志不该算进这一轮的启动耗时。
    param([string]$Root, [int]$Index)
    $base = Join-Path $Root ("sample{0:D2}" -f $Index)
    $dirs = [pscustomobject]@{
        base         = $base
        appdata      = Join-Path $base "appdata"
        localappdata = Join-Path $base "localappdata"
        data         = Join-Path $base "data"
        temp         = Join-Path $base "temp"
    }
    foreach ($d in @($dirs.appdata, $dirs.localappdata, $dirs.data, $dirs.temp)) {
        New-Item -ItemType Directory -Path $d -Force | Out-Null
    }
    return $dirs
}

function Get-IsolationEvidence {
    # 找出"这次启动确实用了这次采样给的数据目录"的证据。
    # 证据只能是 RCGUI_DATA_DIR 指向的目录里、由进程写下的**文件**：目录本身是
    # New-SampleDirs 建的，存在与否跟程序有没有读这个变量毫无关系；只要拿目录当
    # 证据，任何一次启动都会通过，隔离就等于没做。
    # APPDATA 下的子项单独数一份，只作为兼容统计：认 RCGUI_DATA_DIR 的程序不会往
    # 那里写东西，所以它大于零说明跑的是旧版本（没认这个变量），而不是"隔离成功"。
    # LOCALAPPDATA / TEMP 一概不看：.NET 运行时会自己建 Microsoft\Windows\Caches，
    # 跟被测程序做没做事无关。
    #
    # -Exclude：脚本自己在数据目录里预置的文件（相对路径），必须列进来。可写的空模板
    # 文件也是脚本写的，算成证据就等于自己给自己发合格证。
    param([string]$DataDir, [string]$AppData, [string[]]$Exclude = @())

    $skip = @{}
    foreach ($name in $Exclude) {
        if ($name) { $skip[$name.ToLowerInvariant()] = $true }
    }

    $files = New-Object System.Collections.ArrayList
    if (Test-Path -LiteralPath $DataDir) {
        $found = @(Get-ChildItem -LiteralPath $DataDir -Recurse -File -Force -ErrorAction SilentlyContinue)
        foreach ($f in $found) {
            # 空文件不算：程序打开又没写内容的占位文件不能证明它读过这份配置。
            if ($f.Length -le 0) { continue }
            $relative = $f.FullName.Substring($DataDir.Length).TrimStart('\', '/')
            if ($skip.ContainsKey($relative.ToLowerInvariant())) { continue }
            [void]$files.Add($relative)
        }
    }

    $appDataChildren = 0
    if (Test-Path -LiteralPath $AppData) {
        $appDataChildren = @(Get-ChildItem -LiteralPath $AppData -Force -ErrorAction SilentlyContinue).Count
    }

    return [pscustomobject]@{
        Files           = $files.ToArray()
        AppDataChildren = $appDataChildren
    }
}

function Get-OwnedProcessIds {
    # 从本体 PID 往下找出所有子孙进程（受管内核 rclone.exe 就在这一层）。
    # 按 parent 关系找，不按进程名：这台机器上可能还有别人起的 rclone。
    param([int]$RootId)
    $ids = New-Object System.Collections.Generic.List[int]
    if (-not $RootId) { return $ids }
    $ids.Add($RootId)
    $all = @(Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId -ErrorAction SilentlyContinue)
    $changed = $true
    while ($changed) {
        $changed = $false
        foreach ($p in $all) {
            if ($ids.Contains([int]$p.ParentProcessId) -and -not $ids.Contains([int]$p.ProcessId)) {
                $ids.Add([int]$p.ProcessId)
                $changed = $true
            }
        }
    }
    return $ids
}

function Stop-OwnedProcesses {
    # 先子后父地停掉这棵树。本体在托盘里直接 Kill 会把内核留成孤儿，所以顺序不能反。
    # 返回停完仍活着的 PID：调用方用它判断"清理干净了没有"，而不是假设干净。
    param([System.Collections.Generic.List[int]]$Ids)
    $left = @()
    if (-not $Ids -or $Ids.Count -eq 0) { return $left }
    for ($k = $Ids.Count - 1; $k -ge 0; $k--) {
        $targetId = $Ids[$k]
        try {
            $p = Get-Process -Id $targetId -ErrorAction Stop
            Stop-Process -Id $targetId -Force -ErrorAction Stop
            [void]$p.WaitForExit(5000)
        } catch {
            # 已经自己退了，不用管
        }
    }
    foreach ($targetId in $Ids) {
        if (Get-Process -Id $targetId -ErrorAction SilentlyContinue) { $left += $targetId }
    }
    return $left
}

$root = Resolve-RepoRoot $RepoRoot
$exePath = Resolve-ExePath $root $Exe
if (-not (Test-Path -LiteralPath $exePath)) {
    Write-Host "找不到可执行文件：$exePath" -ForegroundColor Red
    Write-Host "先构建（scripts/build.ps1 -Release），或用 -Exe 指定已有产物。" -ForegroundColor Red
    exit 1
}
if ($Samples -lt 1) { Write-Host "-Samples 至少为 1" -ForegroundColor Red; exit 1 }
if ($HealthUrl -and $HealthUrl -notmatch '^https?://') { Write-Host "-HealthUrl 必须是 http(s) URL" -ForegroundColor Red; exit 1 }

$exeItem = Get-Item -LiteralPath $exePath
$exeHash = (Get-FileHash -LiteralPath $exePath -Algorithm SHA256).Hash.ToLower()
$packageBytes = $null
if ($PackageDir) { $packageBytes = Get-DirBytes (Resolve-ExePath $root $PackageDir) }

# 隔离目录的根。默认自己建一个、跑完删掉；-DataRoot 给的是你的目录，脚本不替你删。
$createdDataRoot = $false
if ($DataRoot) {
    $dataRootPath = [System.IO.Path]::GetFullPath((Resolve-ExePath $root $DataRoot))
    if (-not (Test-Path -LiteralPath $dataRootPath)) {
        New-Item -ItemType Directory -Path $dataRootPath -Force | Out-Null
    }
} else {
    $dataRootPath = Join-Path $env:TEMP ("rcgui-perf-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $dataRootPath -Force | Out-Null
    $createdDataRoot = $true
}

if (-not $Json) {
    Write-Step "被测文件：$($exeItem.FullName)"
    Write-Host ("  体积 {0:N0} 字节，sha256 {1}" -f $exeItem.Length, $exeHash)
    if ($null -ne $packageBytes) { Write-Host ("  打包目录 {0:N1} MiB" -f ($packageBytes / 1MB)) }
    Write-Host ("  采样 {0} 次，就绪信号：{1}，超时 {2} ms" -f $Samples, $(if ($HealthUrl) { "GET $HealthUrl" } else { "主窗口出现" }), $StartupTimeoutMs)
    Write-Host "  每次采样用独立数据目录：$dataRootPath"
    Write-Host ""
}

$runs = New-Object System.Collections.ArrayList
# 记录出错的采样次数，只用于最后的提示；成功/失败的统计不靠它，见文件末尾的 $ready / $ok。
$errors = 0

for ($i = 1; $i -le $Samples; $i++) {
    $proc = $null
    $iso = $null
    $record = [ordered]@{
        sample            = $i
        ready_signal      = $null
        startup_ms        = $null
        ready_at_utc      = $null
        settled_at_utc    = $null
        settle_ms         = $null
        working_set_mib   = $null
        private_mib       = $null
        peak_working_mib  = $null
        data_files        = $null
        data_dir_seen     = $null
        appdata_children  = $null
        exited_early      = $false
        error             = $null
    }
    try {
        # 每次采样换一套空目录，把这个进程的数据目录钉进去。否则量到的是
        # "你已经跑着的实例留下的配置和缓存"，数字既不可比也不干净。
        $iso = New-SampleDirs -Root $dataRootPath -Index $i
        $savedEnv = @{
            APPDATA        = $env:APPDATA
            LOCALAPPDATA   = $env:LOCALAPPDATA
            TEMP           = $env:TEMP
            TMP            = $env:TMP
            RCGUI_DATA_DIR = $env:RCGUI_DATA_DIR
            RCLONE_CONFIG  = $env:RCLONE_CONFIG
        }
        # 子进程继承当前进程的环境变量，所以先改、起完立刻改回来。
        # RCLONE_CONFIG 跟着一起钉进隔离目录：否则这一台机器上早就设好的那个变量会
        # 让 rclone 去读真实的 rclone.conf，隔离只挡住了数据目录、没挡住配置。
        $env:APPDATA = $iso.appdata
        $env:LOCALAPPDATA = $iso.localappdata
        $env:RCGUI_DATA_DIR = $iso.data
        $env:RCLONE_CONFIG = Join-Path $iso.data "rclone.conf"
        $env:TEMP = $iso.temp
        $env:TMP = $iso.temp
        try {
            # 秒表在启动之前开：进程创建本身就是启动开销的一部分，Start-Process 之后
            # 再开表会把这段从数字里抹掉。用 Stopwatch 而不是墙钟差，避免系统时间调整干扰。
            $watch = [System.Diagnostics.Stopwatch]::StartNew()
            $proc = Start-Process -FilePath $exePath -PassThru
        } finally {
            foreach ($name in @($savedEnv.Keys)) {
                if ($null -eq $savedEnv[$name]) {
                    Remove-Item "env:$name" -ErrorAction SilentlyContinue
                } else {
                    Set-Item "env:$name" -Value $savedEnv[$name]
                }
            }
        }
        $signal = Wait-UntilReady -Process $proc -Url $HealthUrl -TimeoutMs $StartupTimeoutMs
        $watch.Stop()
        $readyAt = Get-Date

        if ($signal -eq "exited") {
            $record.exited_early = $true
            $record.error = "进程在就绪前退出（退出码 $($proc.ExitCode)）"
            $errors++
        } elseif ($null -eq $signal) {
            $record.error = "等待就绪超过 $StartupTimeoutMs ms"
            $errors++
        } else {
            $record.ready_signal = $signal
            $record.startup_ms = [math]::Round($watch.Elapsed.TotalMilliseconds, 1)
            $record.ready_at_utc = $readyAt.ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ss.fffZ")
            $record.settle_ms = [math]::Max($SettleMs, 0)
            # 等隔离证据：看 RCGUI_DATA_DIR 指向的目录里有没有程序自己写下的文件。
            # 这段等待单独算，不计入 settle——证据来得早晚不该改变读内存的时刻。
            $evidenceDeadline = $readyAt.AddMilliseconds([math]::Max($SettleMs, 0) + 5000)
            $evidence = $null
            while ($true) {
                $evidence = Get-IsolationEvidence -DataDir $iso.data -AppData $iso.appdata
                if (@($evidence.Files).Count -gt 0) { break }
                if ((Get-Date) -ge $evidenceDeadline) { break }
                Start-Sleep -Milliseconds 250
            }
            $record.appdata_children = $evidence.AppDataChildren
            if (@($evidence.Files).Count -eq 0) {
                # 没出现就不记账：这种情况要么连到了别处已经存在的实例，要么这个程序
                # 根本没读 RCGUI_DATA_DIR，那时量出来的数同样不可比。
                $record.startup_ms = $null
                $record.data_files = 0
                $hint = ""
                if ($evidence.AppDataChildren -gt 0) {
                    $hint = "；但它往 APPDATA 隔离目录里写了 $($evidence.AppDataChildren) 项，说明这个 exe 没认 RCGUI_DATA_DIR（旧版本），本次没法证明是干净实例"
                }
                $record.error = "RCGUI_DATA_DIR 指向的目录里没有程序写下的文件（$($iso.data)），确认不了这次是干净实例，数字不记$hint"
                $errors++
            } else {
                $record.data_files = @($evidence.Files).Count
                $record.data_dir_seen = (@($evidence.Files) -join "; ")
                # 就绪之后必须走满 settle 再读内存。证据早早出现也照样等满，
                # 否则量到的是启动瞬间的分配尖峰，而不是稳定占用。
                $settleLeft = ($readyAt.AddMilliseconds([math]::Max($SettleMs, 0)) - (Get-Date)).TotalMilliseconds
                if ($settleLeft -gt 0) { Start-Sleep -Milliseconds ([int][math]::Ceiling($settleLeft)) }
                $record.settled_at_utc = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ss.fffZ")
                $proc.Refresh()
                if (-not $proc.HasExited) {
                    $record.working_set_mib = [math]::Round($proc.WorkingSet64 / 1MB, 1)
                    $record.private_mib = [math]::Round($proc.PrivateMemorySize64 / 1MB, 1)
                    $record.peak_working_mib = [math]::Round($proc.PeakWorkingSet64 / 1MB, 1)
                } else {
                    # 启动时间是真的测到了，只是内存没读到；这条采样不算成功，也不算完全没测到。
                    $record.error = "读内存前进程已退出"
                    $errors++
                }
            }
        }
    } catch {
        $record.error = $_.Exception.Message
        $errors++
    } finally {
        # 只停这次采样自己拉起来的进程树：本体 + 受管内核 rclone.exe，先子后父。
        # 本体可能自己退了，也可能还蹲在托盘里，所以按启动时抓到的 PID 树处理，
        # 不能只看 HasExited——那样会把内核留成孤儿。
        if ($proc) {
            $ownedIds = Get-OwnedProcessIds -RootId $proc.Id
            $leftOver = Stop-OwnedProcesses -Ids $ownedIds
            if ($leftOver.Count -gt 0) {
                $extra = "清理后仍有进程存活：$($leftOver -join ', ')"
                $record.error = $(if ($record.error) { "$($record.error)；$extra" } else { $extra })
                $errors++
            }
        }
    }
    [void]$runs.Add($record)
    if (-not $Json -and $record.startup_ms -ne $null) {
        Write-Host ("  第 {0} 次：启动 {1,7} ms  工作集 {2,7} MiB  私有 {3,7} MiB" -f `
            $record.sample, $record.startup_ms, $record.working_set_mib, $record.private_mib)
    } elseif (-not $Json) {
        Write-Host ("  第 {0} 次：失败——{1}" -f $record.sample, $record.error) -ForegroundColor Yellow
    }
    # 两次采样之间留点间隔，避免上一轮的文件缓存完全命中。
    if ($i -lt $Samples) { Start-Sleep -Milliseconds 500 }
}

# 两个口径分开数：$ready 是"等到就绪、拿到了启动时间"，$ok 是"连内存也读到了"。
# 这样既不会把没测到的数字当 0，也不会让同一次采样同时算进成功和失败。
$ready = @($runs | Where-Object { $null -ne $_.startup_ms })
$ok = @($runs | Where-Object { $null -ne $_.startup_ms -and $null -ne $_.working_set_mib })
$result = [ordered]@{
    measured_at_utc   = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    machine           = "$env:COMPUTERNAME"
    os                = [System.Environment]::OSVersion.VersionString
    exe_path          = $exeItem.FullName
    exe_bytes         = [int64]$exeItem.Length
    exe_sha256        = $exeHash
    package_dir       = $(if ($PackageDir) { (Resolve-ExePath $root $PackageDir) } else { $null })
    package_bytes     = $packageBytes
    data_root         = $dataRootPath
    data_root_owned   = $createdDataRoot
    data_root_removed = $false
    samples_isolated  = @($runs | Where-Object { $_.data_dir_seen }).Count
    # 隔离怎么判的要说清楚，读 JSON 的人才知道 samples_isolated 不是"目录存在"。
    isolation         = [ordered]@{
        data_dir_override = "RCGUI_DATA_DIR"
        config_override   = "RCLONE_CONFIG → <隔离目录>\rclone.conf；环境里原有的值会被顶掉并复原，免得子进程去读真实的 rclone.conf"
        evidence          = "该目录下由被测程序写下的非空文件（配置/日志/缓存）；脚本自己预置的文件不算"
        appdata_compat    = "APPDATA 隔离目录的子项数，只作兼容统计：认 RCGUI_DATA_DIR 的程序不会往那里写"
        samples_with_files = @($runs | Where-Object { $null -ne $_.data_files -and [int]$_.data_files -gt 0 }).Count
    }
    # 计时与内存的口径：启动时间含进程创建（秒表在 Start-Process 之前开），
    # 内存是在就绪后走满 settle_ms 才读的稳定值，不是启动尖峰。
    timing            = [ordered]@{
        startup_includes_process_creation = $true
        memory_read_after_settle_ms       = [math]::Max($SettleMs, 0)
    }
    ready_signal      = $(if ($HealthUrl) { "http" } else { "window" })
    startup_timeout_ms = $StartupTimeoutMs
    settle_ms         = $SettleMs
    samples_requested = $Samples
    samples_ok        = $ok.Count
    samples_ready     = $ready.Count
    samples_failed    = ($Samples - $ok.Count)
    startup_ms        = [ordered]@{
        min    = (Get-Extreme -Values @($ready | ForEach-Object { $_.startup_ms }) -Kind "min")
        median = (Get-Median -Values @($ready | ForEach-Object { $_.startup_ms }))
        max    = (Get-Extreme -Values @($ready | ForEach-Object { $_.startup_ms }) -Kind "max")
    }
    working_set_mib   = [ordered]@{
        median = (Get-Median -Values @($ok | ForEach-Object { $_.working_set_mib }))
        max    = (Get-Extreme -Values @($ok | ForEach-Object { $_.working_set_mib }) -Kind "max")
    }
    private_mib       = [ordered]@{
        median = (Get-Median -Values @($ok | ForEach-Object { $_.private_mib }))
        max    = (Get-Extreme -Values @($ok | ForEach-Object { $_.private_mib }) -Kind "max")
    }
    runs              = $runs
}

# 自己建的隔离目录跑完删掉；-DataRoot 给的是用户的目录、-KeepData 是要留着看，都不删。
# 删之前先把路径证明一遍：绝对路径、在 %TEMP% 之下、叶子名是 rcgui-perf-*，
# 三条都满足才动手——写错一个变量就删别人目录的事不能发生。
if ($createdDataRoot -and -not $KeepData) {
    $fullRoot = [System.IO.Path]::GetFullPath($dataRootPath)
    $tempPrefix = [System.IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    $leafName = Split-Path -Path $fullRoot -Leaf
    $inside = $fullRoot.StartsWith($tempPrefix, [System.StringComparison]::OrdinalIgnoreCase)
    $named = $leafName -like "rcgui-perf-*"
    if ($inside -and $named) {
        try {
            Remove-Item -LiteralPath $fullRoot -Recurse -Force -ErrorAction Stop
            $result.data_root_removed = $true
        } catch {
            Write-Host "  隔离目录没删干净：$($_.Exception.Message)" -ForegroundColor Yellow
        }
    } else {
        Write-Host "  拒绝删除隔离目录（路径不在预期位置）：$fullRoot" -ForegroundColor Yellow
    }
}

if ($Json) {
    $result | ConvertTo-Json -Depth 6
} else {
    Write-Host ""
    if ($ready.Count -eq 0) {
        Write-Host "所有采样都失败，没有可用数字。" -ForegroundColor Red
    } else {
        Write-Step "汇总（成功 $($ok.Count)/$Samples）"
        Write-Host ("  启动     最小 {0} ms / 中位 {1} ms / 最大 {2} ms" -f `
            $result.startup_ms.min, $result.startup_ms.median, $result.startup_ms.max)
        Write-Host ("  工作集   中位 {0} MiB / 最大 {1} MiB" -f `
            $result.working_set_mib.median, $result.working_set_mib.max)
        Write-Host ("  私有内存 中位 {0} MiB / 最大 {1} MiB" -f `
            $result.private_mib.median, $result.private_mib.max)
        Write-Host ""
        Write-Host "把这些数字按 docs/PERFORMANCE.md 的格式记下来；不要手改成更好看的数。" -ForegroundColor DarkGray
    }
    if ($errors -gt 0) {
        Write-Host ("有 {0} 次采样出错（含只测到一半的），原因见上面每行的说明。" -f $errors) -ForegroundColor Yellow
    }
}

# 只有"启动时间和内存都读到"才算一次可用采样；否则退出码 2，让调用方知道数字不能用。
if ($ok.Count -eq 0) { exit 2 }
exit 0
