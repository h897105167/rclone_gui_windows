# 构建 RCGUI（Windows）
#
# 三步，顺序有依赖：插件资产 → 内核 → 编本体。
#   1. 插件资产 = crates/app/assets/plugins/<id>/（config.json + main.wasm + check.sha256），
#      由 scripts/fetch_plugins.ps1 按 plugins/manifest.json 从各插件仓库的 Release 下载并校验 sha256。
#      本仓库不含任何插件源码。
#   2. 内核 = target/<profile>/core/rclone.exe，由 scripts/fetch_core.ps1 按
#      packaging/core-manifest.json 的固定版本下载并校验 sha256（连 MIT 许可证全文一起取）。
#      放在 target 下是为了让 target/<profile>/rclone-gui.exe 和发布包长得一样：
#      程序按 <exe>/core/rclone.exe 找内核，找不到才去别处找，所以开发时也能验证受管内核这条路。
#   3. 应用本体。
#
# 随包插件在 exe 同级的 assets/plugins（程序只认这个位置）；首次运行会把它们释放到
# 数据目录的 plugins/，之后与用户自己装的插件同构，因此可以卸载、也可以「恢复内置」。
#
# 关于内核：RCGUI 默认自己拉起并管理一个 rclone（受管内核），用的是上面这份固定版本；
# 也支持连接你自己启动的外部 rclone（外部端点），那种模式下 RCGUI 只连接，不启动也不停止它。
#
# 用法：
#   .\scripts\build.ps1                  # debug 构建
#   .\scripts\build.ps1 -Release         # release 构建
#   .\scripts\build.ps1 -Run             # 构建后直接运行
#   .\scripts\build.ps1 -SkipPlugins     # 不下载插件（资产缺失会警告；打包时仍会逐项校验）
#   .\scripts\build.ps1 -SkipCore        # 不下载内核（本地已有别的内核位置时可跳过）
#   .\scripts\build.ps1 -CoreZip <zip>   # 用手头的内核 zip（仍会校验 sha256）

param(
    [switch]$Release,
    [switch]$Run,
    [switch]$SkipPlugins,
    [switch]$ForcePlugins,
    [switch]$SkipCore,
    [string]$CoreZip = ""
)

# 注意：这里**不能**设 $ErrorActionPreference = "Stop"。
# PowerShell 5.1 下 cargo 往 stderr 打的 warning 会被当成终止性错误，
# 导致正常构建被误判为失败。改为显式检查 $LASTEXITCODE。
$ErrorActionPreference = "Continue"

function Invoke-Native {
    param([scriptblock]$Command, [string]$What)
    # 先清零：子脚本成功时只写 Write-Host，不会设置 $LASTEXITCODE，
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
$coreDir = Join-Path $repo "target\$profileDir\core"

# 1) 插件资产（从各插件仓库 Release 拉，按 manifest 的 sha256 校验）
if ($SkipPlugins) {
    Write-Host "`n==> [1/3] 跳过插件资产下载（-SkipPlugins）" -ForegroundColor Yellow
} else {
    Write-Host "`n==> [1/3] 准备插件资产（plugins/manifest.json）..." -ForegroundColor Cyan
    $fetch = Join-Path $PSScriptRoot "fetch_plugins.ps1"
    $fetchArgs = @()
    if ($ForcePlugins) { $fetchArgs += "-Force" }
    Invoke-Native { & $fetch @fetchArgs } "插件资产准备"
}

# 2) 内核（固定版本 + sha256，落在 target/<profile>/core）
if ($SkipCore) {
    Write-Host "`n==> [2/3] 跳过内核下载（-SkipCore）" -ForegroundColor Yellow
} else {
    Write-Host "`n==> [2/3] 准备 rclone 内核（packaging/core-manifest.json）..." -ForegroundColor Cyan
    $fetchCore = Join-Path $PSScriptRoot "fetch_core.ps1"
    $coreArgs = @{ Dest = "target\$profileDir\core" }
    if ($CoreZip) { $coreArgs["LocalZip"] = $CoreZip }
    Invoke-Native { & $fetchCore @coreArgs } "内核准备"
}

# 3) 构建应用
Write-Host "`n==> [3/3] 构建应用 (rclone-gui)..." -ForegroundColor Cyan
Invoke-Native { cargo build -p rclone-gui @profileArgs } "应用构建"

$exe = Join-Path $repo "target\$profileDir\rclone-gui.exe"
Write-Host "`n==> 构建完成：$exe" -ForegroundColor Green
Write-Host "    体积：$([Math]::Round((Get-Item $exe).Length / 1MB, 1)) MB" -ForegroundColor Green

$coreExe = Join-Path $coreDir "rclone.exe"
if (Test-Path $coreExe) {
    Write-Host "    内核：$coreExe" -ForegroundColor Green
} else {
    Write-Host "    提示：exe 同级没有 core/rclone.exe，这次跑起来只能用外部端点（加不上 -SkipCore 重跑即可取内核）" -ForegroundColor Yellow
}

$assets = Join-Path $repo "target\$profileDir\assets\plugins"
if (Test-Path $assets) {
    Write-Host "    随包插件：$((Get-ChildItem $assets -Directory | Select-Object -ExpandProperty Name) -join ', ')" -ForegroundColor Green
} else {
    Write-Host "    警告：exe 同级没有 assets/plugins，随包插件不会被释放（先跑 scripts/fetch_plugins.ps1）" -ForegroundColor Yellow
}

if ($Run) {
    Write-Host "`n==> 启动 rclone-gui..." -ForegroundColor Cyan
    & $exe
}
