# 按 plugins/manifest.json 下载并校验插件包，解到 crates/app/assets/plugins/<dir>/
#
# 本仓库**不存**插件源码：每个插件是独立仓库，Release 里挂打包好的 zip。
# 这个脚本负责「锁文件 → 本地资产」这一步，构建前跑一次即可。
#
# 校验分两层：
#   1. zip 的 sha256 与 manifest 里写的一致（防传输损坏 / 换包）
#   2. 解开后再按包内 check.sha256 逐文件核对（防解开时丢文件）
# 通过后写 <dir>/.fetched.json 记下 tag + sha256，下次同版本直接跳过。
#
# 用法：
#   .\scripts\fetch_plugins.ps1            # 缺什么下什么
#   .\scripts\fetch_plugins.ps1 -Force     # 全部重下
#   .\scripts\fetch_plugins.ps1 -Id imagehost
#   .\scripts\fetch_plugins.ps1 -Offline   # 只检查本地资产齐不齐（CI/离线构建）
#
# 环境变量 RCGUI_SKIP_PLUGINS=1 等价于 -Offline，方便在没网时先跑构建。

param(
    [switch]$Force,
    [switch]$Offline,
    [string]$Id
)

$ErrorActionPreference = "Continue"

function Fail([string]$msg) {
    Write-Host "✗ $msg" -ForegroundColor Red
    exit 1
}

function Get-Sha256([string]$path) {
    (Get-FileHash -Path $path -Algorithm SHA256).Hash.ToLower()
}

if ($env:RCGUI_SKIP_PLUGINS -eq "1") { $Offline = $true }

# 定位仓库根目录（本脚本位于 <repo>/scripts/）；$PSScriptRoot 在 -File 与点源两种
# 调用方式下都可用，这里再兜一层 $MyInvocation，避免被 scriptblock 调起时为空。
$here = $PSScriptRoot
if (-not $here) { $here = Split-Path -Parent $MyInvocation.MyCommand.Path }
if (-not $here) { $here = (Get-Location).Path }
$repo = Split-Path -Parent $here
Set-Location $repo

$manifestPath = Join-Path $repo "plugins/manifest.json"
# 兜底：被 scriptblock 调起时 $PSScriptRoot 可能为空，向上找 manifest 所在目录
$probe = $repo
for ($i = 0; $i -lt 5 -and -not (Test-Path $manifestPath); $i++) {
    $probe = Split-Path -Parent $probe
    if (-not $probe) { break }
    $manifestPath = Join-Path $probe "plugins/manifest.json"
    if (Test-Path $manifestPath) { $repo = $probe; Set-Location $repo }
}
if (-not (Test-Path $manifestPath)) { Fail "找不到 $manifestPath" }
$manifest = Get-Content $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
$destRoot = Join-Path $repo "crates/app/assets/plugins"

$list = @($manifest.plugins)
if ($Id) {
    $list = @($list | Where-Object { $_.id -eq $Id })
    if ($list.Count -eq 0) { Fail "manifest 里没有 id=$Id" }
}

$tmpRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("rcgui-plugins-" + [guid]::NewGuid().ToString("n"))
New-Item -ItemType Directory -Force -Path $tmpRoot | Out-Null

$failed = @()
try {
    foreach ($p in $list) {
        $dir = if ($p.dir) { $p.dir } else { $p.id }
        $dest = Join-Path $destRoot $dir
        $marker = Join-Path $dest ".fetched.json"

        # 已经下过同一个版本 → 跳过
        if (-not $Force -and (Test-Path $marker)) {
            $old = Get-Content $marker -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($old.tag -eq $p.tag -and $old.sha256 -eq $p.sha256) {
                Write-Host "= $($p.id) $($p.tag) 已就绪（$dir）" -ForegroundColor DarkGray
                continue
            }
        }

        if ($Offline) {
            $failed += "$($p.id)（离线模式，本地缺 $dest）"
            continue
        }

        $url = "$($p.repo)/releases/download/$($p.tag)/$($p.asset)"
        $zip = Join-Path $tmpRoot $p.asset
        Write-Host "↓ $($p.id) $($p.tag) ← $url" -ForegroundColor Cyan
        try {
            Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing -ErrorAction Stop
        } catch {
            $failed += "$($p.id)：下载失败（$($_.Exception.Message)）"
            continue
        }

        $got = Get-Sha256 $zip
        if ($got -ne $p.sha256.ToLower()) {
            $failed += "$($p.id)：sha256 不符（期望 $($p.sha256)，实际 $got）"
            continue
        }

        $unzip = Join-Path $tmpRoot ("x-" + $p.id)
        if (Test-Path $unzip) { Remove-Item $unzip -Recurse -Force }
        New-Item -ItemType Directory -Force -Path $unzip | Out-Null
        try {
            Expand-Archive -Path $zip -DestinationPath $unzip -Force -ErrorAction Stop
        } catch {
            $failed += "$($p.id)：解压失败（$($_.Exception.Message)）"
            continue
        }

        # 包内 check.sha256：逐文件核对（入口必须在表里，这一条宿主也会再查一遍）
        $checkFile = Join-Path $unzip "check.sha256"
        if (-not (Test-Path $checkFile)) {
            $failed += "$($p.id)：包内缺 check.sha256"
            continue
        }
        $bad = $null
        foreach ($line in Get-Content $checkFile -Encoding UTF8) {
            $t = $line.Trim()
            if (-not $t) { continue }
            $parts = $t -split '\s+', 2
            if ($parts.Count -ne 2) { $bad = "check.sha256 行格式不对：$t"; break }
            $want = $parts[0].ToLower()
            $rel = $parts[1].TrimStart('*')
            $file = Join-Path $unzip $rel
            if (-not (Test-Path $file)) { $bad = "包内缺文件：$rel"; break }
            if ((Get-Sha256 $file) -ne $want) { $bad = "文件哈希不符：$rel"; break }
        }
        if ($bad) { $failed += "$($p.id)：$bad"; continue }

        if (Test-Path $dest) { Remove-Item $dest -Recurse -Force }
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        Copy-Item (Join-Path $unzip "*") $dest -Recurse -Force
        $markerObj = [ordered]@{
            id       = $p.id
            tag      = $p.tag
            asset    = $p.asset
            sha256   = $p.sha256.ToLower()
            repo     = $p.repo
            fetched  = (Get-Date).ToString("s")
        }
        $markerObj | ConvertTo-Json | Set-Content -Path $marker -Encoding UTF8
        Write-Host "✓ $($p.id) $($p.tag) → $dest" -ForegroundColor Green
    }
} finally {
    if (Test-Path $tmpRoot) { Remove-Item $tmpRoot -Recurse -Force -ErrorAction SilentlyContinue }
}

if ($failed.Count -gt 0) {
    Write-Host ""
    Write-Host "以下插件没准备好：" -ForegroundColor Red
    foreach ($f in $failed) { Write-Host "  - $f" -ForegroundColor Red }
    exit 1
}

Write-Host ""
Write-Host "插件资产已就绪：$destRoot" -ForegroundColor Green
