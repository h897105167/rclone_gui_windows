# 构建 RCGUI（Windows）
#
# 关键顺序：先拿到插件资产，再构建应用。
# 插件资产 = crates/app/assets/plugins/<id>/（config.json + main.wasm + check.sha256），
# 由 scripts/fetch_plugins.ps1 按 plugins/manifest.json 从各插件仓库的 Release 下载。
# 本仓库不含任何插件源码，所以「构建」= 拉插件 + 编本体。
#
# 内置插件（随应用分发的）：见 plugins/manifest.json。首次运行释放到 <data>/plugins/，
# 之后与用户自己装的插件在磁盘上完全同构，因此可以卸载、也可以「恢复内置」。
#
# 说明：RCGUI 不再内嵌 rclone.exe。用户需要自行安装 rclone 并以 --rc 启动，
# RCGUI 只连接该 RC endpoint（不启动、不停止 rclone 进程）。
#
# 用法：
#   .\scripts\build.ps1              # debug 构建
#   .\scripts\build.ps1 -Release     # release 构建
#   .\scripts\build.ps1 -Run         # 构建后直接运行
#   .\scripts\build.ps1 -SkipPlugins # 不检查/不下载插件（改动不涉及插件时用；资产缺失会警告）

param(
    [switch]$Release,
    [switch]$Run,
    [switch]$SkipPlugins,
    [switch]$ForcePlugins
)

# 注意：这里**不能**设 $ErrorActionPreference = "Stop"。
# PowerShell 5.1 下 cargo 往 stderr 打的 warning 会被当成终止性错误，
# 导致正常构建被误判为失败。改为显式检查 $LASTEXITCODE。
$ErrorActionPreference = "Continue"

function Invoke-Native {
    param([scriptblock]$Command, [string]$What)
    # 先清零：fetch_plugins.ps1 成功时只写 Write-Host，不会设置 $LASTEXITCODE，
    # 残留值或 $null 会让 `$LASTEXITCODE -ne 0` 把成功误判成失败。
    $global:LASTEXITCODE = 0
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$What 失败（退出码 $LASTEXITCODE）"
    }
}

# 定位仓库根目录（本脚本位于 <repo>/scripts/）
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

# 确保 cargo 可用（未加入 PATH 时用默认安装位置）
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
    if (Test-Path (Join-Path $cargoBin "cargo.exe")) {
        $env:PATH = "$cargoBin;$env:PATH"
    } else {
        throw "找不到 cargo。请先安装 Rust 工具链：https://rustup.rs"
    }
}

$profileArgs = @()
if ($Release) { $profileArgs += "--release" }
$profileDir = if ($Release) { "release" } else { "debug" }

# 1) 插件资产（从各插件仓库 Release 拉，按 manifest 的 sha256 校验）
if ($SkipPlugins) {
    Write-Host "`n==> [1/2] 跳过插件资产检查（-SkipPlugins）" -ForegroundColor Yellow
} else {
    Write-Host "`n==> [1/2] 准备插件资产（plugins/manifest.json）..." -ForegroundColor Cyan
    $fetch = Join-Path $PSScriptRoot "fetch_plugins.ps1"
    $fetchArgs = @()
    if ($ForcePlugins) { $fetchArgs += "-Force" }
    Invoke-Native { & $fetch @fetchArgs } "插件资产准备"
}

# 2) 构建应用
Write-Host "`n==> [2/2] 构建应用 (rclone-gui)..." -ForegroundColor Cyan
Invoke-Native { cargo build -p rclone-gui @profileArgs } "应用构建"

$exe = Join-Path $repo "target\$profileDir\rclone-gui.exe"
Write-Host "`n==> 构建完成：$exe" -ForegroundColor Green
Write-Host "    体积：$([Math]::Round((Get-Item $exe).Length / 1MB, 1)) MB" -ForegroundColor Green

$assets = Join-Path $repo "target\$profileDir\assets\plugins"
if (Test-Path $assets) {
    Write-Host "    内置插件：$((Get-ChildItem $assets -Directory | Select-Object -ExpandProperty Name) -join ', ')" -ForegroundColor Green
} else {
    Write-Host "    警告：exe 同级没有 assets/plugins，内置插件不会被释放（先跑 scripts/fetch_plugins.ps1）" -ForegroundColor Yellow
}

if ($Run) {
    Write-Host "`n==> 启动 rclone-gui..." -ForegroundColor Cyan
    & $exe
}
