# 打包插件目录：config.json + main.wasm → check.sha256
#
# 插件三件套：
#   config.json   —— 元数据（id / name / version / entry / tags / capabilities / plugin_setting）
#   main.wasm     —— 主脚本（WASI preview1，stdin 一个 JSON、stdout 一个 JSON）
#   check.sha256  —— 清单，安装与加载时逐文件校验（sha256sum 格式：小写哈希 + 两个空格 + 相对路径）
#
# 用法：
#   .\scripts\pack-plugin.ps1 -Name oneclick -Wasn target\wasm32-wasip1\release\oneclick.wasm
#   .\scripts\pack-plugin.ps1 -Name oneclick -Wasn ... -Out crates\app\assets\plugins\onerclone
#
# 说明：check.sha256 必须**最后**生成，且生成前要先清掉旧文件，
# 否则清单会包含自己的上一版内容（校验时必然失败）。

param(
    [Parameter(Mandatory = $true)][string]$Name,
    [Parameter(Mandatory = $true)][string]$Wasn,
    [string]$Out = "",
    [string]$Version = ""
)

$ErrorActionPreference = "Continue"

$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

$srcDir = Join-Path $repo "plugins\$Name"
if (-not (Test-Path $srcDir)) { throw "找不到插件源目录: $srcDir" }

$configPath = Join-Path $srcDir "config.json"
if (-not (Test-Path $configPath)) { throw "找不到 $configPath" }

$wasmPath = if ([IO.Path]::IsPathRooted($Wasn)) { $Wasn } else { Join-Path $repo $Wasn }
if (-not (Test-Path $wasmPath)) { throw "找不到 wasm: $wasmPath" }

$cfg = Get-Content $configPath -Raw -Encoding UTF8 | ConvertFrom-Json
if ($Version -ne "") { $cfg.version = $Version }

# 输出目录：默认 crates/app/assets/plugins/<id>（内置插件随 exe 发布的位置）
if ($Out -eq "") { $Out = "crates\app\assets\plugins\$($cfg.id)" }
$outDir = if ([IO.Path]::IsPathRooted($Out)) { $Out } else { Join-Path $repo $Out }

if (Test-Path $outDir) { Remove-Item $outDir -Recurse -Force }
New-Item -ItemType Directory -Path $outDir -Force | Out-Null

# entry 一般就是 main.wasm；wasm 一律按 entry 的名字落盘
$entry = $cfg.entry
if ([string]::IsNullOrWhiteSpace($entry)) { $entry = "main.wasm" }
Copy-Item $wasmPath (Join-Path $outDir $entry) -Force

# config.json：没改版本号就原样复制（保留手写格式），改了才重新序列化
if ($Version -eq "") {
    Copy-Item $configPath (Join-Path $outDir "config.json") -Force
} else {
    $json = $cfg | ConvertTo-Json -Depth 12
    [IO.File]::WriteAllText(
        (Join-Path $outDir "config.json"),
        $json + "`n",
        (New-Object System.Text.UTF8Encoding($false))
    )
}

# 生成 check.sha256：除自身外每个文件一行，按路径排序，哈希小写
$lines = @()
Get-ChildItem $outDir -Recurse -File |
    Where-Object { $_.Name -ne "check.sha256" } |
    ForEach-Object {
        $rel = $_.FullName.Substring($outDir.Length + 1).Replace('\', '/')
        $hash = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        $lines += "$hash  $rel"
    }
$lines = $lines | Sort-Object
[IO.File]::WriteAllText(
    (Join-Path $outDir "check.sha256"),
    (($lines -join "`n") + "`n"),
    (New-Object System.Text.UTF8Encoding($false))
)

Write-Host "已打包插件 $($cfg.id) v$($cfg.version) → $outDir" -ForegroundColor Green
Get-ChildItem $outDir -File | ForEach-Object {
    Write-Host ("    {0,-16} {1,10:N0} 字节" -f $_.Name, $_.Length)
}
