# 构建 RCGUI（Windows）
#
# 关键顺序：必须先构建 WASM 插件，再构建应用。
# 原因：crates/app/src/main.rs 用 include_bytes!(env!("IMAGEHOST_WASM")) 把插件
# 编译进程序，而 crates/app/build.rs 是去 target/wasm32-wasip1/<profile>/imagehost.wasm
# 找这个文件的。顺序反了 build.rs 会直接报错（这是刻意加的构建期护栏）。
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

# 1) 先构建 WASM 插件
Write-Host "`n==> [1/2] 构建 WASM 插件 (imagehost)..." -ForegroundColor Cyan
Invoke-Native { cargo build -p imagehost-plugin --target wasm32-wasip1 @profileArgs } "WASM 插件构建"

# 2) 再构建应用（此时 build.rs 能找到 imagehost.wasm）
Write-Host "`n==> [2/2] 构建应用 (rclone-gui)..." -ForegroundColor Cyan
Invoke-Native { cargo build -p rclone-gui @profileArgs } "应用构建"

$profileDir = if ($Release) { "release" } else { "debug" }
$exe = Join-Path $repo "target\$profileDir\rclone-gui.exe"
Write-Host "`n==> 构建完成：$exe" -ForegroundColor Green
Write-Host "    体积：$([Math]::Round((Get-Item $exe).Length / 1MB, 1)) MB" -ForegroundColor Green

if ($Run) {
    Write-Host "`n==> 启动 rclone-gui..." -ForegroundColor Cyan
    & $exe
}
