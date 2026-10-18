# 构建 RCGUI（Windows）
#
# 关键顺序：必须先构建 WASM 插件，再构建应用。
# 原因：crates/app/src/main.rs 用 include_bytes!(env!("IMAGEHOST_WASM")) 把 imagehost
# 编进程序，而 crates/app/build.rs 是去 target/wasm32-wasip1/<profile>/imagehost.wasm
# 找这个文件的。顺序反了 build.rs 会直接报错（这是刻意加的构建期护栏）。
#
# 内置插件分两类：
#   imagehost  —— 单文件 .wasm，编进程序，首次运行释放到 <data>/plugins/imagehost.wasm
#   oneclick   —— 三件套（config.json / check.sha256 / main.wasm），打包到
#                 crates/app/assets/plugins/<id>/，再由 build.rs 复制到 exe 同级
#                 assets/plugins/，首次运行时释放到 <data>/plugins/。
#                 它和用户自己装的插件在磁盘上完全同构，因此可以卸载。
#
# 说明：RCGUI 不再内嵌 rclone.exe。用户需要自行安装 rclone 并以 --rc 启动，
# RCGUI 只连接该 RC endpoint（不启动、不停止 rclone 进程）。
#
# 用法：
#   .\scripts\build.ps1              # debug 构建
#   .\scripts\build.ps1 -Release     # release 构建
#   .\scripts\build.ps1 -Run         # 构建后直接运行

param(
    [switch]$Release,
    [switch]$Run
)

# 注意：这里**不能**设 $ErrorActionPreference = "Stop"。
# PowerShell 5.1 下 cargo 往 stderr 打的 warning 会被当成终止性错误，
# 导致正常构建被误判为失败。改为显式检查 $LASTEXITCODE。
$ErrorActionPreference = "Continue"

function Invoke-Native {
    param([scriptblock]$Command, [string]$What)
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

# 确保 wasm32-wasip1 目标已安装（插件需要）
$installed = (rustup target list --installed) -join "`n"
if ($installed -notmatch "wasm32-wasip1") {
    Write-Host "==> 安装 wasm32-wasip1 目标..." -ForegroundColor Yellow
    Invoke-Native { rustup target add wasm32-wasip1 } "安装 wasm32-wasip1"
}

$profileArgs = @()
if ($Release) { $profileArgs += "--release" }
$profileDir = if ($Release) { "release" } else { "debug" }

# WASM 插件**永远**用 release 构建，与应用 profile 无关。
#
# 原因：插件产物会被打包进 crates/app/assets/plugins/ 并随仓库提交（这样直接
# `cargo build -p rclone-gui` 也能拿到内置插件）。debug 版 wasm 有 5 MB 且每次
# 编译都变，会让这个提交的二进制文件反复抖动；而插件的调试通常用日志就够，
# 真要调插件再手动 `cargo build -p oneclick-plugin --target wasm32-wasip1`。
$wasmProfileArgs = @("--release")
$wasmProfileDir = "release"

# 1) 先构建 WASM 插件
Write-Host "`n==> [1/3] 构建 WASM 插件 (imagehost)..." -ForegroundColor Cyan
Invoke-Native { cargo build -p imagehost-plugin --target wasm32-wasip1 @wasmProfileArgs } "imagehost 插件构建"

Write-Host "`n==> [2/3] 构建 WASM 插件 (oneclick)..." -ForegroundColor Cyan
Invoke-Native { cargo build -p oneclick-plugin --target wasm32-wasip1 @wasmProfileArgs } "oneclick 插件构建"

# 打包三件套到 assets/plugins/<id>/（config.json + main.wasm + check.sha256）
Write-Host "`n==> 打包内置插件 oneclick..." -ForegroundColor Cyan
Invoke-Native {
    & (Join-Path $PSScriptRoot "pack-plugin.ps1") `
        -Name oneclick `
        -Wasn "target\wasm32-wasip1\$wasmProfileDir\oneclick.wasm"
} "打包 oneclick 插件"

# 2) 再构建应用（此时 build.rs 能找到 imagehost.wasm 与 assets/plugins）
Write-Host "`n==> [3/3] 构建应用 (rclone-gui)..." -ForegroundColor Cyan
Invoke-Native { cargo build -p rclone-gui @profileArgs } "应用构建"

$exe = Join-Path $repo "target\$profileDir\rclone-gui.exe"
Write-Host "`n==> 构建完成：$exe" -ForegroundColor Green
Write-Host "    体积：$([Math]::Round((Get-Item $exe).Length / 1MB, 1)) MB" -ForegroundColor Green

$assets = Join-Path $repo "target\$profileDir\assets\plugins"
if (Test-Path $assets) {
    Write-Host "    内置插件：$((Get-ChildItem $assets -Directory | Select-Object -ExpandProperty Name) -join ', ')" -ForegroundColor Green
} else {
    Write-Host "    警告：exe 同级没有 assets/plugins，内置插件不会被释放" -ForegroundColor Yellow
}

if ($Run) {
    Write-Host "`n==> 启动 rclone-gui..." -ForegroundColor Cyan
    & $exe
}
