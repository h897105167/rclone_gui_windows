<#
.SYNOPSIS
    打一个可发布的 RCGUI 版本：portable zip、安装包 exe、SHA256SUMS。

.DESCRIPTION
    产物命名（版本和架构只出现在这一处）：
        dist\RCGUI-<version>-windows-x64-portable.zip
        dist\RCGUI-<version>-windows-x64-setup.exe
        dist\SHA256SUMS

    版本号只有一个来源：Cargo.toml 里 [workspace.package] 的 version。
    传 -Version 只是为了让"打错版本号"当场失败，不能用来临时改名。

    portable zip 的内容（解包后就在根上，不再套一层目录）：
        rclone-gui.exe
        core\rclone.exe            按 packaging\core-manifest.json 固定版本，校验过 sha256
        core\rclone-COPYING.txt    rclone 的许可证全文
        core\.fetched.json         内核的版本与哈希，NOTICE 里的版本号就指这里的
        assets\plugins\<id>\...    随包插件，装到 exe 同一层
        portable                   空文件，运行时靠它把数据放到 exe 旁边的 data\
        licenses\NOTICE.txt        第三方来源说明
        SHA256SUMS                 各文件自己的 sha256

    assets\plugins 而不是 plugins：程序启动时只从 exe 同级的 assets\plugins 读随包插件
    （crates\app\src\plugins.rs::builtin_root）。放在别的目录等于没装，首次运行也不会
    把插件释放到用户插件目录。内核仍然是 core\rclone.exe（crates\app\src\core.rs 定的），
    这两条路径不通用，改一个不改另一个就会出一份装不上的包。

    插件清单是白名单：只打 plugins\manifest.json 里逐项列出的插件，目录名、版本、sha256
    都必须和清单一致。不扫描 crates\app\assets\plugins 下的其它目录——那里的残留在
    release 里出现属于发版事故，不是"顺手带上"。

    安装包不写用户数据：数据仍在 %APPDATA%\RCGUI，安装与卸载都不碰它。

.PARAMETER Version
    期望的版本号，只用于核对 Cargo.toml。不一致直接失败。

.PARAMETER OutDir
    产物目录，默认 dist（相对仓库根）。这个目录会被当作"可以清空 staging 子目录"的边界，
    所以不能是仓库根，也不能是盘符根。

.PARAMETER SkipPlugins
    跳过下载插件，但仍然校验手上已有的插件是否与清单一致。跳过不等于免检。

.PARAMETER SkipCore
    跳过下载内核，但仍然要求 core\rclone.exe 与许可证全文已经就位。

.PARAMETER SkipBuild
    跳过 cargo build，直接用 target\release\rclone-gui.exe。

.PARAMETER SkipInstaller
    不生成安装包（本机没装 Inno Setup 时用）。此时只有 portable zip。

.PARAMETER IsccPath
    ISCC.exe 的路径，默认从 PATH 和 Inno Setup 6 的安装目录里找。

.PARAMETER CoreZip
    本地已有的 rclone 内核 zip，传给 fetch_core.ps1（仍然校验 sha256）。

.PARAMETER CoreDir
    已就位内核所在目录，默认依次看 target\release\core 和 core。

.EXAMPLE
    .\scripts\build-release.ps1

.EXAMPLE
    .\scripts\build-release.ps1 -SkipBuild -SkipInstaller

.NOTES
    退出码：0 成功；1 任何一步失败（校验失败不会降级成警告）。
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$OutDir = "dist",
    [switch]$SkipPlugins,
    [switch]$SkipCore,
    [switch]$SkipBuild,
    [switch]$SkipInstaller,
    [string]$IsccPath = "",
    [string]$CoreZip = "",
    [string]$CoreDir = "",
    # 额外排除的插件 id。清单里没有的插件本来就不会进包，这里只是排查时临时少打一个；
    # 写了个不存在的 id 不报错（清单才是白名单），但会在输出里说一声。
    [string[]]$ExcludePlugins = @()
)

function Write-Step {
    param([string]$Text)
    Write-Host "==> $Text" -ForegroundColor Cyan
}

function Fail {
    param([string]$Text)
    Write-Host "错误：$Text" -ForegroundColor Red
    exit 1
}

function Get-Sha256 {
    param([string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLower()
}

function Resolve-RepoRoot {
    $dir = $PSScriptRoot
    if (-not $dir) { $dir = Split-Path -Parent $MyInvocation.MyCommand.Path }
    if (-not $dir) { $dir = (Get-Location).Path }
    for ($i = 0; $i -lt 6 -and $dir; $i++) {
        if (Test-Path -LiteralPath (Join-Path $dir "Cargo.toml")) { return $dir }
        $parent = Split-Path -Parent $dir
        if ($parent -eq $dir) { break }
        $dir = $parent
    }
    Fail "找不到仓库根（向上找了 6 层没看到 Cargo.toml）。"
}

function Get-WorkspaceVersion {
    param([string]$CargoToml)
    $raw = Get-Content -LiteralPath $CargoToml -Raw -Encoding UTF8
    # 只看 [workspace.package] 这一段，避免误抓某个依赖的 version。
    if ($raw -notmatch '(?ms)^\[workspace\.package\](.*?)(?=^\[|\z)') {
        Fail "Cargo.toml 里没有 [workspace.package] 段。"
    }
    $section = $Matches[1]
    if ($section -notmatch '(?m)^\s*version\s*=\s*"([^"]+)"') {
        Fail "[workspace.package] 里没有 version = `"...`"。"
    }
    return $Matches[1]
}

# PE 版本资源只吃数字。0.3.0 补成 0.3.0.0，0.3.0-rc.1 也补成 0.3.0.0：
# 预发布后缀留在 AppVersion（人看的、文件名的）里，不进这个字段。
function Get-NumericVersion {
    param([string]$Version)
    if ($Version -notmatch '^(\d+)\.(\d+)\.(\d+)') {
        Fail "版本 $Version 不是 主.次.修订 开头，没法生成数字版本号。"
    }
    $maj = [int]$Matches[1]; $min = [int]$Matches[2]; $pat = [int]$Matches[3]
    foreach ($n in @($maj, $min, $pat)) {
        if ($n -gt 65535) { Fail "版本 $Version 有一段超过 65535，Windows 版本资源装不下。" }
    }
    return "$maj.$min.$pat.0"
}

function Find-Iscc {
    param([string]$Explicit)
    if ($Explicit) {
        if (Test-Path -LiteralPath $Explicit) { return $Explicit }
        Fail "指定的 ISCC 不存在：$Explicit"
    }
    $cmd = Get-Command iscc.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $candidates = @(
        (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
        (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe"),
        (Join-Path ${env:LOCALAPPDATA} "Programs\Inno Setup 6\ISCC.exe")
    )
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) { return $candidate }
    }
    return ""
}

# 删 staging 目录前先证明"它确实是这次该删的那个目录"。
# 只靠 Join-Path 拼出来的路径去 Remove-Item -Recurse 是不安全的：OutDir 传错一个值，
# 或者中途被替换成符号链接，删掉的就是别的东西，而且是静默的。
function Remove-StagingTree {
    param(
        [string]$Path,
        [string]$ExpectedName,
        [string]$OutDirFull,
        [string]$RepoFull
    )

    $full = [System.IO.Path]::GetFullPath($Path)

    if ((Split-Path -Leaf $full) -ne $ExpectedName) {
        Fail "拒绝删除 $full：目录名不是约定的 $ExpectedName。"
    }
    if ((Split-Path -Parent $full) -ne $OutDirFull) {
        Fail "拒绝删除 $full：它不在产物目录 $OutDirFull 下。"
    }
    if ($OutDirFull -eq $RepoFull) {
        Fail "拒绝删除：产物目录就是仓库根（$RepoFull）。-OutDir 指到仓库根会把源码一起清掉。"
    }
    $outRoot = [System.IO.Path]::GetPathRoot($OutDirFull)
    if ($OutDirFull -eq $outRoot -or $OutDirFull -eq (Split-Path -Parent $RepoFull)) {
        Fail "拒绝删除：产物目录 $OutDirFull 是盘符根或仓库的父目录。"
    }
    if (-not (Test-Path -LiteralPath $full)) { return }

    # 路径上任何一段是 reparse point（符号链接、junction、挂载点）都不删：
    # 删的是链接目标的内容，而链接是谁建的、指向哪儿都不由这个脚本保证。
    $current = $full
    while ($current) {
        $item = Get-Item -LiteralPath $current -Force -ErrorAction SilentlyContinue
        if ($item -and ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
            Fail "拒绝删除 $full：路径上的 $current 是符号链接/junction，删它等于删别处的东西。"
        }
        $parent = Split-Path -Parent $current
        if ($parent -eq $current -or -not $parent) { break }
        $current = $parent
    }

    Write-Host "    清理旧 staging：$full" -ForegroundColor DarkGray
    Remove-Item -LiteralPath $full -Recurse -Force
}

$repoRoot = Resolve-RepoRoot
$cargoToml = Join-Path $repoRoot "Cargo.toml"
$workspaceVersion = Get-WorkspaceVersion $cargoToml

if ($workspaceVersion -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$') {
    Fail "Cargo.toml 的版本 '$workspaceVersion' 不是 SemVer（形如 0.3.0 或 0.3.0-rc.1）。"
}
if ($Version -and $Version -ne $workspaceVersion) {
    Fail "命令行给的 -Version $Version 与 Cargo.toml 的 $workspaceVersion 不一致。版本号只从 Cargo.toml 来（见 docs/VERSIONING.md）。"
}

$version = $workspaceVersion
$numericVersion = Get-NumericVersion $version
$outDirPath = [System.IO.Path]::GetFullPath($(if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $repoRoot $OutDir }))
$repoFull = [System.IO.Path]::GetFullPath($repoRoot)
$stagingName = "RCGUI-$version-windows-x64-portable"
$stagingDir = Join-Path $outDirPath $stagingName
$zipPath = Join-Path $outDirPath "$stagingName.zip"
$setupPath = Join-Path $outDirPath "RCGUI-$version-windows-x64-setup.exe"
$sumsPath = Join-Path $outDirPath "SHA256SUMS"

Write-Step "版本 $version（数字版本 $numericVersion），产物目录 $outDirPath"

# ---- 1. 插件：按清单下载并校验 ----
if (-not $SkipPlugins) {
    Write-Step "取插件（按 plugins/manifest.json 固定版本，sha256 必须匹配）"
    & (Join-Path $repoRoot "scripts\fetch_plugins.ps1")
    if ($LASTEXITCODE -ne 0) { Fail "fetch_plugins.ps1 失败（退出码 $LASTEXITCODE）。插件哈希对不上时不会继续打包。" }
}

# ---- 2. 构建 ----
if (-not $SkipBuild) {
    Write-Step "构建 release"
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $cargo) {
        $fallback = Join-Path $env:USERPROFILE ".cargo\bin"
        if (Test-Path -LiteralPath $fallback) { $env:PATH = "$fallback;$env:PATH" }
        $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    }
    if (-not $cargo) { Fail "找不到 cargo。先装 Rust 工具链，或把 %USERPROFILE%\.cargo\bin 加进 PATH。" }

    & cargo build -p rclone-gui --release
    if ($LASTEXITCODE -ne 0) { Fail "cargo build --release 失败（退出码 $LASTEXITCODE）。" }
}

$exeSource = Join-Path $repoRoot "target\release\rclone-gui.exe"
if (-not (Test-Path -LiteralPath $exeSource)) {
    Fail "没有 target\release\rclone-gui.exe。先跑 cargo build -p rclone-gui --release，或去掉 -SkipBuild。"
}

# ---- 3. 内核：下载或复用已就位的那份 ----
$coreCandidates = @()
if ($CoreDir) {
    $coreCandidates += $(if ([System.IO.Path]::IsPathRooted($CoreDir)) { $CoreDir } else { Join-Path $repoRoot $CoreDir })
}
$coreCandidates += (Join-Path $repoRoot "target\release\core")
$coreCandidates += (Join-Path $repoRoot "core")

if (-not $SkipCore) {
    Write-Step "取 rclone 内核（固定版本 + sha256 校验）"
    $fetchArgs = @{ Dest = "target\release\core" }
    if ($CoreZip) { $fetchArgs["LocalZip"] = $CoreZip }
    & (Join-Path $repoRoot "scripts\fetch_core.ps1") @fetchArgs
    if ($LASTEXITCODE -ne 0) { Fail "fetch_core.ps1 失败（退出码 $LASTEXITCODE）。内核没通过校验，不要打包。" }
}

$coreDirPath = $null
foreach ($candidate in $coreCandidates) {
    if (Test-Path -LiteralPath (Join-Path $candidate "rclone.exe")) { $coreDirPath = $candidate; break }
}
if (-not $coreDirPath) {
    Fail @"
没找到 rclone.exe。找过这些目录：
$(($coreCandidates | ForEach-Object { "  $_" }) -join "`r`n")
先跑 `.\scripts\fetch_core.ps1 -Dest target\release\core`（或 `.\scripts\build.ps1 -Release` 会自动做），
或者用 -CoreDir 指到已有内核，或者用 -CoreZip 传官方 zip（仍会校验 sha256）。
没有内核的包启动后只能连外部端点，不能当受管内核用，所以这里不降级。
"@
}
$coreExe = Join-Path $coreDirPath "rclone.exe"
$coreCopying = Join-Path $coreDirPath "COPYING"
$coreMarker = Join-Path $coreDirPath ".fetched.json"
Write-Host "    内核目录：$coreDirPath" -ForegroundColor DarkGray

# ---- 3b. 内核内容核对：以固定清单为准，不拿 marker 自报的哈希当依据 ----
# 这一步对 -SkipCore 同样生效：跳过的是下载，不是校验。
Write-Step "核对内核内容与固定清单"
$coreManifestPath = Join-Path $repoRoot "packaging\core-manifest.json"
if (-not (Test-Path -LiteralPath $coreManifestPath)) {
    Fail "找不到 packaging\core-manifest.json。内核的固定哈希就在这个文件里，没有它没法核对。"
}
try {
    $coreManifest = Get-Content -LiteralPath $coreManifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
} catch {
    Fail "packaging\core-manifest.json 解析失败：$($_.Exception.Message)"
}
$pinned = $coreManifest.core
if (-not $pinned) { Fail "core-manifest.json 里没有 core 段。" }
foreach ($field in @("version", "exe_sha256", "license_sha256")) {
    if (-not $pinned.$field) { Fail "core-manifest.json 缺少 core.$field，无法核对内核。" }
}

# marker 只用来确认这份内核确实是 fetch_core 按这个清单取下来的；
# 它自己写下的哈希不作依据，说了算的是清单里的固定值。
if (-not (Test-Path -LiteralPath $coreMarker)) {
    Fail @"
内核目录 $coreDirPath 里没有 .fetched.json，这份内核的来路不明。
发布包里的内核必须能追溯到固定版本，所以"来源已经不清楚"的内核不能用来打包。
用 `.\scripts\fetch_core.ps1 -Dest target\release\core` 重新取一份（或用 -CoreZip 传官方 zip）。
"@
}
$markerText = Get-Content -LiteralPath $coreMarker -Raw -Encoding UTF8 -ErrorAction SilentlyContinue
if (-not $markerText -or -not $markerText.Trim()) {
    Fail "内核目录 $coreDirPath 里的 .fetched.json 是空文件。空记录等于没有记录，不能据此发布。"
}
try {
    $coreMeta = $markerText | ConvertFrom-Json
} catch {
    Fail "$coreMarker 不是合法 JSON：$($_.Exception.Message)"
}
if ($coreMeta.version -ne $pinned.version) {
    Fail "内核记录的版本是 $($coreMeta.version)，清单固定的是 $($pinned.version)。换了内核就要同步更新 manifest。"
}
if ($coreMeta.exe_sha256 -ne $pinned.exe_sha256) {
    Fail "内核记录里的 exe_sha256 与清单不符：记录 $($coreMeta.exe_sha256)，清单 $($pinned.exe_sha256)。"
}
if ($coreMeta.license_sha256 -and $coreMeta.license_sha256 -ne $pinned.license_sha256) {
    Fail "内核记录里的 license_sha256 与清单不符：记录 $($coreMeta.license_sha256)，清单 $($pinned.license_sha256)。"
}

$exeShaActual = Get-Sha256 $coreExe
if ($exeShaActual -ne $pinned.exe_sha256) {
    Fail @"
rclone.exe 的实际 sha256 与清单不符：
  实际 $exeShaActual
  清单 $($pinned.exe_sha256)
这个 exe 不是清单固定的那一个，打出来的包不可复现。重跑 fetch_core.ps1 再打包。
"@
}
$exeBytesActual = (Get-Item -LiteralPath $coreExe).Length
if ($pinned.exe_bytes -and $exeBytesActual -ne [int64]$pinned.exe_bytes) {
    Fail "rclone.exe 大小与清单不符：实际 $exeBytesActual 字节，清单 $($pinned.exe_bytes) 字节。"
}
if (-not (Test-Path -LiteralPath $coreCopying)) {
    Fail @"
内核目录 $coreDirPath 里没有 COPYING（rclone 的 MIT 许可证全文）。
MIT 要求随二进制分发许可证，NOTICE 也会指到 core\rclone-COPYING.txt，缺了就出一份不合规的包。
用 fetch_core.ps1 重取一次（它会按清单里的 license_url 取），或手工放进 $coreDirPath\COPYING。
"@
}
$licenseShaActual = Get-Sha256 $coreCopying
if ($licenseShaActual -ne $pinned.license_sha256) {
    Fail @"
COPYING 的实际 sha256 与清单不符：
  实际 $licenseShaActual
  清单 $($pinned.license_sha256)
许可证全文被换过就不再是 rclone 发布的那一份，不能这样发。重跑 fetch_core.ps1。
"@
}
Write-Host "    内核 exe  $exeShaActual（与清单一致）" -ForegroundColor DarkGray
Write-Host "    许可证    $licenseShaActual（与清单一致）" -ForegroundColor DarkGray

# ---- 4. 插件白名单：只打清单里列出的 ----
Write-Step "按清单挑插件（白名单，不扫目录）"
$manifestPath = Join-Path $repoRoot "plugins\manifest.json"
if (-not (Test-Path -LiteralPath $manifestPath)) { Fail "找不到 plugins\manifest.json。" }
try {
    $manifest = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
} catch {
    Fail "plugins\manifest.json 解析失败：$($_.Exception.Message)"
}
$manifestPlugins = @($manifest.plugins)
if ($manifestPlugins.Count -eq 0) { Fail "plugins\manifest.json 里一个插件都没有，打出来的包没法用。" }

$assetsRoot = Join-Path $repoRoot "crates\app\assets\plugins"
# onerclone（商店里的 oneclick）无条件排除：它会自己拉起 rclone 进程，
# 和 0.3.0 起由应用管理的受管内核抢同一个 RC 端口。它现在也不在
# plugins\manifest.json 里，所以这一行是兜底——将来有人把它加回清单，
# 便携包仍然不会带上它。-ExcludePlugins 只是再加几项，不能把它减掉。
$excludedPlugins = @($ExcludePlugins) + @("onerclone")
$stagedPlugins = @()
$skippedPlugins = @()

foreach ($p in $manifestPlugins) {
    if ($excludedPlugins -contains $p.id) {
        Write-Host "    跳过 $($p.id)（-ExcludePlugins）" -ForegroundColor Yellow
        $skippedPlugins += $p.id
        continue
    }
    $srcDir = Join-Path $assetsRoot $p.dir
    if (-not (Test-Path -LiteralPath $srcDir)) {
        Fail @"
清单里的插件 $($p.id) 没有取下来：缺目录 crates\app\assets\plugins\$($p.dir)。
这是构建资产（不是用户插件目录），跑 .\scripts\fetch_plugins.ps1 会按 plugins\manifest.json 下载并校验。
$($(if ($SkipPlugins) { "注意：这次带了 -SkipPlugins，跳过的是下载，不是校验——缺包仍然算失败。" } else { "" }))
"@
    }

    $configPath = Join-Path $srcDir "config.json"
    $wasmPath = Join-Path $srcDir "main.wasm"
    if (-not (Test-Path -LiteralPath $configPath) -or -not (Test-Path -LiteralPath $wasmPath)) {
        Fail "插件目录 $($p.dir) 缺 config.json 或 main.wasm。先跑 scripts\fetch_plugins.ps1 -Force 重新校验。"
    }
    try {
        $pluginConfig = Get-Content -LiteralPath $configPath -Raw -Encoding UTF8 | ConvertFrom-Json
    } catch {
        Fail "插件 $($p.dir) 的 config.json 解析失败：$($_.Exception.Message)"
    }
    if ($pluginConfig.id -ne $p.dir -or $pluginConfig.id -ne $p.id) {
        Fail "插件目录 $($p.dir) / 清单 id $($p.id) / config.json id $($pluginConfig.id) 三者不一致。"
    }
    if ($pluginConfig.version -ne $p.tag.TrimStart("v")) {
        Fail "插件 $($p.id) 的 config.json 版本 $($pluginConfig.version) 与清单 tag $($p.tag) 不一致。"
    }

    # .fetched.json 是 fetch_plugins.ps1 写的取件凭据：tag 与 sha256 必须和清单对得上。
    # 少了它说明这个目录不是按清单取下来的（可能是旧版本残留，或手工拷进来的）。
    $markerPath = Join-Path $srcDir ".fetched.json"
    if (-not (Test-Path -LiteralPath $markerPath)) {
        Fail "插件 $($p.id) 缺 .fetched.json：这个目录不是按清单拉下来的。跑 .\scripts\fetch_plugins.ps1 -Force。"
    }
    try {
        $marker = Get-Content -LiteralPath $markerPath -Raw -Encoding UTF8 | ConvertFrom-Json
    } catch {
        Fail "插件 $($p.id) 的 .fetched.json 解析失败：$($_.Exception.Message)"
    }
    if ($marker.sha256 -ne $p.sha256) {
        Fail "插件 $($p.id) 取件时的 sha256 $($marker.sha256) 与清单 $($p.sha256) 不一致。重新 fetch，不要手工改清单。"
    }
    if ($marker.tag -ne $p.tag) {
        Fail "插件 $($p.id) 取件时的 tag $($marker.tag) 与清单 $($p.tag) 不一致。"
    }

    # 包自带的 check.sha256 再核一遍：取下来之后文件被改过（或者解压坏了）也要当场发现。
    $checkPath = Join-Path $srcDir "check.sha256"
    $checkedFiles = 0
    if (Test-Path -LiteralPath $checkPath) {
        foreach ($line in (Get-Content -LiteralPath $checkPath -Encoding UTF8)) {
            if (-not $line.Trim()) { continue }
            if ($line -notmatch '^\s*([0-9a-fA-F]{64})\s+\*?(.+?)\s*$') {
                Fail "插件 $($p.id) 的 check.sha256 有一行格式不对：$line"
            }
            $want = $Matches[1].ToLower()
            $rel = $Matches[2]
            $filePath = Join-Path $srcDir $rel
            if (-not (Test-Path -LiteralPath $filePath)) { Fail "插件 $($p.id) 缺文件 $rel（check.sha256 里列着）。" }
            $got = Get-Sha256 $filePath
            if ($got -ne $want) { Fail "插件 $($p.id) 的 $rel 哈希对不上：check.sha256 是 $want，实际是 $got。" }
            $checkedFiles++
        }
    } else {
        Write-Host "    提示：$($p.id) 没有 check.sha256，只按清单 sha256 与 .fetched.json 校验" -ForegroundColor Yellow
    }

    $stagedPlugins += "$($p.id) $($pluginConfig.version)（校验 $checkedFiles 个文件）"
}

if ($stagedPlugins.Count -eq 0) {
    Fail "没有可打包的插件：插件全是这次被 -ExcludePlugins 排掉的。"
}

# ---- 5. 整理 portable 目录 ----
Write-Step "整理 portable 目录"
$outRootCheck = [System.IO.Path]::GetPathRoot($outDirPath)
if ($outDirPath -eq $repoFull -or $outDirPath -eq $outRootCheck) {
    Fail "拒绝整理产物目录：$outDirPath 是仓库根或盘符根。-OutDir 要指向一个专门放产物的目录。"
}
Remove-StagingTree -Path $stagingDir -ExpectedName $stagingName -OutDirFull $outDirPath -RepoFull $repoFull
New-Item -ItemType Directory -Path $stagingDir -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $stagingDir "licenses") -Force | Out-Null

Copy-Item -LiteralPath $exeSource -Destination (Join-Path $stagingDir "rclone-gui.exe") -Force

$stagedCoreDir = Join-Path $stagingDir "core"
New-Item -ItemType Directory -Path $stagedCoreDir -Force | Out-Null
Copy-Item -LiteralPath $coreExe -Destination (Join-Path $stagedCoreDir "rclone.exe") -Force
Copy-Item -LiteralPath $coreCopying -Destination (Join-Path $stagedCoreDir "rclone-COPYING.txt") -Force
# marker 已经在 3b 里验过存在且与清单一致，这里照抄进包；缺了就是前面漏了检查。
if (-not (Test-Path -LiteralPath $coreMarker)) {
    Fail "内核目录 $coreDirPath 没有 .fetched.json，包里的内核来源信息会缺；前面的核对不该放过这种状态。"
}
Copy-Item -LiteralPath $coreMarker -Destination (Join-Path $stagedCoreDir ".fetched.json") -Force

# 复制之后再量一次：拷错文件、磁盘写入被截断都会在这里露出来，
# 而不是等到用户装完发现内核跑不起来。
$stagedExeSha = Get-Sha256 (Join-Path $stagedCoreDir "rclone.exe")
if ($stagedExeSha -ne $pinned.exe_sha256) {
    Fail "复制进包里的 rclone.exe sha256 变了：$stagedExeSha，期望 $($pinned.exe_sha256)。检查磁盘或杀软是否改写了文件。"
}
$stagedLicenseSha = Get-Sha256 (Join-Path $stagedCoreDir "rclone-COPYING.txt")
if ($stagedLicenseSha -ne $pinned.license_sha256) {
    Fail "复制进包里的 rclone-COPYING.txt sha256 变了：$stagedLicenseSha，期望 $($pinned.license_sha256)。"
}

$stagedPluginsDir = Join-Path $stagingDir "assets\plugins"
New-Item -ItemType Directory -Path $stagedPluginsDir -Force | Out-Null
foreach ($p in $manifestPlugins) {
    if ($excludedPlugins -contains $p.id) { continue }
    Copy-Item -LiteralPath (Join-Path $assetsRoot $p.dir) -Destination (Join-Path $stagedPluginsDir $p.dir) -Recurse -Force
}

# portable 标记：空文件。应用看到它就把数据放到 exe 旁边的 data\ 而不是 %APPDATA%。
New-Item -ItemType File -Path (Join-Path $stagingDir "portable") -Force | Out-Null

# NOTICE 只写包里真实存在的文件，指到不存在的东西等于给用户一条假线索。
$notice = @(
    "RCGUI $version portable",
    "",
    "启动：双击 rclone-gui.exe。"
    "内核：core\rclone.exe，版本与哈希见 core\.fetched.json（默认用它做受管内核，也可以在设置里改成连外部端点）。"
    "插件：assets\plugins\<id>\，每个包的版本与来源见该包自己的 config.json。"
    "数据：这个目录里的 data\（由同级的 portable 空标记决定），不会写到 %APPDATA%。",
    "",
    "第三方组件：",
    "  rclone   MIT 许可，全文见 core\rclone-COPYING.txt。",
    "  插件     各自仓库与版本见 assets\plugins\<id>\config.json；随包插件清单见仓库 plugins\manifest.json。",
    "",
    "本程序（RCGUI）尚未声明项目许可证，见仓库根目录的说明。",
    "校验：同目录 SHA256SUMS 记录了两个发布文件的 sha256，发布页给出同一份。"
) -join "`r`n"
[System.IO.File]::WriteAllText((Join-Path $stagingDir "licenses\NOTICE.txt"), $notice, (New-Object System.Text.UTF8Encoding($false)))

# NOTICE 里提到的包内文件必须真的在包里。
foreach ($required in @("core\rclone.exe", "core\rclone-COPYING.txt", "licenses\NOTICE.txt")) {
    if (-not (Test-Path -LiteralPath (Join-Path $stagingDir $required))) {
        Fail "包里缺 $required，但 NOTICE/安装脚本会引用它。"
    }
}

# 敏感信息闸门：产物里不该出现开发机路径、令牌、内嵌密码。
Write-Step "扫描产物里的路径与凭据"
$textExtensions = @(".txt", ".json", ".md", ".conf", ".toml", ".yml", ".yaml", ".log", ".ps1", ".cmd", ".bat")
$patterns = @(
    @{ Name = "绝对路径（盘符 + Users）"; Regex = '[A-Za-z]:\\\\?[Uu]sers\\\\?' },
    @{ Name = "GitHub 令牌"; Regex = 'gh[pousr]_[A-Za-z0-9]{20,}' },
    @{ Name = "GitHub 细粒度令牌"; Regex = 'github_pat_[A-Za-z0-9_]{20,}' },
    @{ Name = "AWS 访问密钥"; Regex = 'AKIA[0-9A-Z]{16}' },
    @{ Name = "私钥"; Regex = 'BEGIN [A-Z ]*PRIVATE KEY' }
)
$scanned = 0
$hits = @()
foreach ($file in (Get-ChildItem -LiteralPath $stagingDir -Recurse -File)) {
    if ($textExtensions -notcontains $file.Extension.ToLower()) { continue }
    $scanned++
    $text = Get-Content -LiteralPath $file.FullName -Raw -Encoding UTF8 -ErrorAction SilentlyContinue
    if (-not $text) { continue }
    foreach ($pattern in $patterns) {
        if ($text -match $pattern.Regex) {
            $hits += "$($file.FullName) 命中「$($pattern.Name)」"
        }
    }
}
if ($hits.Count -gt 0) {
    foreach ($hit in $hits) { Write-Host "    $hit" -ForegroundColor Red }
    Fail "产物里出现了不该带出去的内容（上面这些）。先清理再打包。"
}
Write-Host "    检查了 $scanned 个文本文件，没有命中" -ForegroundColor Green

Write-Step "压缩 portable zip"
if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
Compress-Archive -Path (Join-Path $stagingDir "*") -DestinationPath $zipPath -CompressionLevel Optimal -Force

if (-not $SkipInstaller) {
    Write-Step "生成安装包（Inno Setup）"
    $iscc = Find-Iscc $IsccPath
    if (-not $iscc) {
        Fail @"
找不到 ISCC.exe（Inno Setup 6 的编译器）。装 Inno Setup 6 之后重跑，或者：
  - 只想出 portable zip：加 -SkipInstaller
  - 自己指定编译器：-IsccPath "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"
CI 上由 .github\workflows\release.yml 装好 Inno Setup 再调用，本地没装是正常的。
"@
    }
    $iss = Join-Path $repoRoot "packaging\RCGUI.iss"
    if (-not (Test-Path -LiteralPath $iss)) { Fail "找不到 packaging\RCGUI.iss。" }
    if (Test-Path -LiteralPath $setupPath) { Remove-Item -LiteralPath $setupPath -Force }

    # 中文界面是可选的：Inno Setup 6 的默认安装里没有 Languages\ChineseSimplified.isl
    # （实测本机 28 个语言文件里没有中文），写死这条引用会让整份脚本编译失败——
    # 安装包出不来，而程序本身完全没问题。所以找不到就退回英文界面，
    # 打印一行说明，而不是失败。
    $chineseIsl = ""
    $innoChinese = Join-Path (Split-Path -Parent $iscc) "Languages\ChineseSimplified.isl"
    if (Test-Path -LiteralPath $innoChinese) {
        $chineseIsl = $innoChinese
    } else {
        $repoChinese = Join-Path $repoRoot "packaging\languages\ChineseSimplified.isl"
        if (Test-Path -LiteralPath $repoChinese) { $chineseIsl = $repoChinese }
    }

    # 两个版本号分开传：AppVersion 给人看（可以是 0.3.0-rc.1），
    # AppVersionNumeric 只给 PE 版本资源，必须是纯数字。
    $isccArgs = @(
        "/DAppVersion=$version"
        "/DAppVersionNumeric=$numericVersion"
        "/DSourceDir=$stagingDir"
        "/DOutputDir=$outDirPath"
        "/Qp"
    )
    if ($chineseIsl) {
        $isccArgs += "/DChineseMessagesFile=$chineseIsl"
    } else {
        Write-Host "    提示：没找到 ChineseSimplified.isl，安装程序界面用英文（程序自身界面不受影响）" -ForegroundColor Yellow
        Write-Host "          想装中文向导：把中文语言文件放到 packaging\languages\ChineseSimplified.isl 再打包" -ForegroundColor DarkGray
    }
    $isccArgs += $iss
    & $iscc @isccArgs
    if ($LASTEXITCODE -ne 0) { Fail "ISCC 失败（退出码 $LASTEXITCODE）。" }
    if (-not (Test-Path -LiteralPath $setupPath)) {
        Fail "ISCC 退出码 0，但没有生成 $setupPath。检查 RCGUI.iss 的 OutputBaseFilename。"
    }
} else {
    Write-Host "    跳过安装包（-SkipInstaller），这次只有 portable zip" -ForegroundColor Yellow
}

Write-Step "写 SHA256SUMS"
$artifactNames = @()
if (Test-Path -LiteralPath $setupPath) { $artifactNames += (Split-Path -Leaf $setupPath) }
if (Test-Path -LiteralPath $zipPath) { $artifactNames += (Split-Path -Leaf $zipPath) }
if ($artifactNames.Count -eq 0) { Fail "没有生成任何产物，SHA256SUMS 无从写起。" }

$lines = @()
foreach ($name in ($artifactNames | Sort-Object)) {
    $path = Join-Path $outDirPath $name
    $lines += ("{0}  {1}" -f (Get-Sha256 $path), $name)
}
[System.IO.File]::WriteAllText($sumsPath, (($lines -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding($false)))

Write-Step "完成"
foreach ($name in ($artifactNames | Sort-Object)) {
    $path = Join-Path $outDirPath $name
    $size = (Get-Item -LiteralPath $path).Length
    Write-Host ("    {0}  {1} 字节  sha256 {2}" -f $name, $size, (Get-Sha256 $path))
}
Write-Host "    SHA256SUMS  $sumsPath"
Write-Host "    插件已打包：$($stagedPlugins -join '; ')"
if ($skippedPlugins.Count -gt 0) {
    Write-Host "    这次额外排除了：$($skippedPlugins -join ', ')（产物与发布件不同，别拿去发）" -ForegroundColor Yellow
}
# 只报这次真的用到的开关。全列一遍会让人以为自己传了没传的参数，
# 也可能让 CI 日志里出现"跳过安装包"而实际上安装包已经生成了。
$usedSkips = @()
if ($SkipPlugins)   { $usedSkips += "-SkipPlugins" }
if ($SkipCore)      { $usedSkips += "-SkipCore" }
if ($SkipBuild)     { $usedSkips += "-SkipBuild" }
if ($SkipInstaller) { $usedSkips += "-SkipInstaller" }
if ($usedSkips.Count -gt 0) {
    Write-Host ("    注意：这次用了 {0}。跳过的只是下载或编译，插件与内核仍然逐项校验过；但产物可能不全，发布流水线不用这些开关。" -f ($usedSkips -join " ")) -ForegroundColor Yellow
}
