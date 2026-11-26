<#
.SYNOPSIS
    按 packaging/core-manifest.json 下载固定版本的 rclone 内核，校验后放到 core\ 目录。

.DESCRIPTION
    这个脚本只做一件事：拿一个说得清来路的 rclone.exe。

    校验顺序（任何一步失败都停下，不会降级成警告）：
      1. manifest 自己的 asset_sha256 必须是 64 位十六进制；
      2. 下载到的 zip 字节数和 sha256 必须与 manifest 一致；
      3. 再拿官方 SHA256SUMS 交叉核对同一个文件（该文件是 PGP 明文签名，
         只匹配 "<sha256>  <文件名>" 这种行，不理会 -----BEGIN PGP SIGNED MESSAGE----- 那层）；
      4. 解包出来的 rclone.exe 自己的 sha256（和字节数）必须等于 manifest 的
         exe_sha256 / exe_bytes —— zip 的哈希只说明"压缩包是官方那个"，
         说明不了"里面那个 exe 就是我预期的那个"；
      5. 真的运行一次 `rclone.exe version`，确认版本与 manifest 一致；
      6. 许可证全文要与 manifest 的 license_sha256 一致，否则算失败。

    第 6 步不能省的原因：官方 windows-amd64 zip 里没有 COPYING（v1.76.0 实测只有
    README.txt、rclone.exe、git-log.txt、rclone.1、README.html），而发布包里必须有
    rclone 的许可证全文。所以清单另外钉了 license_url（指向 version 对应的 tag）
    与 license_sha256，许可证按它单独取、单独校验。

    想用本地已有的内核包，传 -LocalZip：仍然走第 1–6 步，只是省掉下载。
    这样 CI 与本地打包用的是同一条校验路径。

.PARAMETER Dest
    内核输出目录，默认 target\core（相对仓库根）。脚本会写 rclone.exe 与 COPYING。
    默认不放在仓库根：以前默认是 core，忘了传 -Dest 就会往仓库根丢一个 85 MB 的
    rclone.exe，而 .gitignore 并没有忽略它，`git add -A` 会把它带进提交。

.PARAMETER Manifest
    固定版本清单，默认 packaging/core-manifest.json。

.PARAMETER LocalZip
    已有内核 zip 的路径。仍然校验 sha256，不接受"信任本地文件"。

.PARAMETER Force
    即使目标目录已有校验通过的内核，也重新取一次。

.PARAMETER SkipChecksumsCrossCheck
    跳过与官方 SHA256SUMS 的交叉核对，只在离线环境下用。manifest 里的 sha256
    依然必需匹配——这个开关不会让未校验的文件过关。

.PARAMETER SkipVersionRun
    不运行 `rclone.exe version`（例如在非 x64 主机上准备产物）。

.EXAMPLE
    .\scripts\fetch_core.ps1
    结果落在 target\core\：rclone.exe、COPYING、.fetched.json 三件。

.EXAMPLE
    .\scripts\fetch_core.ps1 -LocalZip D:\downloads\rclone-v1.76.0-windows-amd64.zip -Force

.NOTES
    退出码：0 成功；1 校验失败或前置条件不满足。
#>
[CmdletBinding()]
param(
    [string]$Dest = "target\core",
    [string]$Manifest = "packaging/core-manifest.json",
    [string]$LocalZip = "",
    [switch]$Force,
    [switch]$SkipChecksumsCrossCheck,
    [switch]$SkipVersionRun
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
        if (Test-Path -LiteralPath (Join-Path $dir "packaging\core-manifest.json")) { return $dir }
        $parent = Split-Path -Parent $dir
        if ($parent -eq $dir) { break }
        $dir = $parent
    }
    Fail "找不到仓库根（向上找了 6 层没看到 packaging\core-manifest.json）。"
}

$repoRoot = Resolve-RepoRoot
$manifestPath = Join-Path $repoRoot $Manifest
if (-not (Test-Path -LiteralPath $manifestPath)) { Fail "找不到固定版本清单：$manifestPath" }

$parsed = $null
try {
    $parsed = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
} catch {
    Fail "解析 $Manifest 失败：$($_.Exception.Message)"
}
if ($parsed.schema_version -ne 1) { Fail "manifest 的 schema_version 应为 1，实际是 $($parsed.schema_version)" }
$core = $parsed.core
if (-not $core) { Fail "manifest 里没有 core 段。" }

foreach ($field in @("version", "asset_name", "asset_url", "asset_sha256", "checksums_url", "license_url", "license_sha256", "exe_sha256")) {
    if (-not $core.$field) { Fail "manifest 缺少字段 core.$field" }
}
if ($core.asset_sha256 -notmatch '^[0-9a-f]{64}$') {
    Fail "manifest 的 core.asset_sha256 不是 64 位小写十六进制：'$($core.asset_sha256)'。请填官方 Release 资产页上的值，不要写占位符。"
}
# exe_sha256 是解包后那个 rclone.exe 的哈希，不是 zip 的哈希。
# 打包脚本会拿它核对实际文件，所以它必须是人工从可信来源量出来的值，
# 不能由这个脚本"下载完算一遍再写进去"——那样等于自己给自己发证书。
if ($core.exe_sha256 -notmatch '^[0-9a-f]{64}$') {
    Fail "manifest 的 core.exe_sha256 不是 64 位小写十六进制：'$($core.exe_sha256)'。"
}
if ($core.asset_url -notmatch '^https://github\.com/rclone/rclone/releases/download/') {
    Fail "core.asset_url 不是 rclone 官方 Release 地址：$($core.asset_url)"
}
if ($core.license_sha256 -notmatch '^[0-9a-f]{64}$') {
    Fail "manifest 的 core.license_sha256 不是 64 位小写十六进制：'$($core.license_sha256)'。"
}
if ($core.license_url -notmatch '^https://raw\.githubusercontent\.com/rclone/rclone/') {
    Fail "core.license_url 不是 rclone 官方仓库的原文地址：$($core.license_url)"
}
if ($core.license_url -notmatch [regex]::Escape("/$($core.version)/")) {
    Fail "core.license_url 没有钉在 $($core.version) 这个 tag 上：$($core.license_url)。指向 master 或 latest 拿到的许可证可能不是这个内核的版本。"
}

$destDir = if ([System.IO.Path]::IsPathRooted($Dest)) { $Dest } else { Join-Path $repoRoot $Dest }
$exePath = Join-Path $destDir "rclone.exe"
$copyingPath = Join-Path $destDir "COPYING"
$markerPath = Join-Path $destDir ".fetched.json"

# 许可证全文单独取：先看 zip 里有没有（将来上游可能补上），哈希对就用包内的；
# 否则按清单钉住的地址下载。大小、哈希任何一项不符都 Fail ——
# 正式发布包必须带许可证全文，这件事没有"警告一下继续"的档位。
# 读 $core 是脚本作用域里的清单值，调用方不用再传一遍。
function Install-License {
    param(
        [string]$DestPath,
        [string]$ExtractDir = ""
    )

    if ($ExtractDir -and (Test-Path -LiteralPath $ExtractDir)) {
        $inZip = Get-ChildItem -LiteralPath $ExtractDir -Recurse -File -Filter "COPYING" -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($inZip) {
            $zipSha = Get-Sha256 $inZip.FullName
            if ($zipSha -eq $core.license_sha256) {
                Copy-Item -LiteralPath $inZip.FullName -Destination $DestPath -Force
                Write-Host "    许可证取自内核包内的 COPYING（$zipSha）"
                return "zip"
            }
            Write-Host "    包内的 COPYING 哈希与清单不符（$zipSha），改用清单地址重新取" -ForegroundColor Yellow
        }
    }

    $licenseTmp = Join-Path $env:TEMP ("rcgui-license-" + [guid]::NewGuid().ToString("N"))
    try {
        Write-Step "取 rclone 许可证 $($core.license_url)"
        try {
            Invoke-WebRequest -Uri $core.license_url -OutFile $licenseTmp -UseBasicParsing
        } catch {
            Fail "取许可证失败：$($_.Exception.Message)。包不带许可证全文就不能发布，这里不跳过。"
        }

        $licenseBytes = (Get-Item -LiteralPath $licenseTmp).Length
        if ($core.license_bytes -and $licenseBytes -ne [int64]$core.license_bytes) {
            Fail "许可证大小与清单不符：实际 $licenseBytes 字节，清单写 $($core.license_bytes) 字节。"
        }
        $licenseSha = Get-Sha256 $licenseTmp
        if ($licenseSha -ne $core.license_sha256) {
            Fail "许可证 sha256 与清单不符：实际 $licenseSha，清单写 $($core.license_sha256)。上游文件变了要重新核对清单里的值，不能直接放过。"
        }

        Copy-Item -LiteralPath $licenseTmp -Destination $DestPath -Force
        Write-Host "    许可证取自 $($core.license_url)（$licenseSha）"
        return "download"
    } finally {
        if (Test-Path -LiteralPath $licenseTmp) {
            Remove-Item -LiteralPath $licenseTmp -Force -ErrorAction SilentlyContinue
        }
    }
}

Write-Step "内核版本 $($core.version)（$($core.platform)-$($core.arch)），目标目录 $destDir"

if ((Test-Path -LiteralPath $exePath) -and -not $Force) {
    $markerOk = $false
    if (Test-Path -LiteralPath $markerPath) {
        try {
            $marker = Get-Content -LiteralPath $markerPath -Raw -Encoding UTF8 | ConvertFrom-Json
            # 拿清单里钉住的哈希核对磁盘上的文件本身。marker 里的 exe_sha256
            # 只是随行记录，旧版本写的是"下载时现算的值"，不能反过来当依据。
            if ($marker.version -eq $core.version -and (Get-Sha256 $exePath) -eq $core.exe_sha256) {
                $markerOk = $true
            }
        } catch {
            $markerOk = $false
        }
    }
    if ($markerOk) {
        # 记录只说明 exe 是对的；许可证不在记录里，得单独确认一次。
        # 旧版本留下的内核目录没有 COPYING，这里只补许可证，不重新下载 30MB。
        $licenseOk = (Test-Path -LiteralPath $copyingPath) -and ((Get-Sha256 $copyingPath) -eq $core.license_sha256)
        if (-not $licenseOk) {
            Write-Step "内核已校验通过，但许可证缺失或哈希不符，只补取许可证"
            Install-License -DestPath $copyingPath | Out-Null
        }
        Write-Step "已有校验通过的内核，跳过下载（要重新取用 -Force）"
        Write-Host "    $exePath"
        exit 0
    }
    Write-Step "已有内核文件但记录对不上，重新取一次"
}

$tmpRoot = Join-Path $env:TEMP ("rcgui-core-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmpRoot -Force | Out-Null

try {
    $zipPath = ""
    if ($LocalZip) {
        $zipPath = if ([System.IO.Path]::IsPathRooted($LocalZip)) { $LocalZip } else { Join-Path $repoRoot $LocalZip }
        if (-not (Test-Path -LiteralPath $zipPath)) { Fail "找不到本地内核包：$zipPath" }
        Write-Step "使用本地内核包 $zipPath（仍然校验 sha256）"
    } else {
        $zipPath = Join-Path $tmpRoot $core.asset_name
        Write-Step "下载 $($core.asset_url)"
        try {
            Invoke-WebRequest -Uri $core.asset_url -OutFile $zipPath -UseBasicParsing
        } catch {
            Fail "下载失败：$($_.Exception.Message)。需要联网，或用 -LocalZip 指定本地已有的内核包。"
        }
    }

    $zipBytes = (Get-Item -LiteralPath $zipPath).Length
    if ($core.asset_bytes -and $zipBytes -ne [int64]$core.asset_bytes) {
        Fail "内核包大小与 manifest 不符：实际 $zipBytes 字节，manifest 写 $($core.asset_bytes) 字节。"
    }

    $actualSha = Get-Sha256 $zipPath
    if ($actualSha -ne $core.asset_sha256) {
        Fail "内核包 sha256 与 manifest 不符：实际 $actualSha，manifest 写 $($core.asset_sha256)。"
    }
    Write-Step "zip sha256 与 manifest 一致：$actualSha"

    if (-not $SkipChecksumsCrossCheck) {
        $sumsPath = Join-Path $tmpRoot "SHA256SUMS"
        $sums = $null
        try {
            Invoke-WebRequest -Uri $core.checksums_url -OutFile $sumsPath -UseBasicParsing
            $sums = Get-Content -LiteralPath $sumsPath -Encoding UTF8
        } catch {
            Fail "取官方 SHA256SUMS 失败：$($_.Exception.Message)。离线时用 -SkipChecksumsCrossCheck（manifest 里的 sha256 仍会校验）。"
        }

        # 官方文件是 PGP 明文签名：前面是 armor，后面是 "<sha256>  <文件名>"。
        # 只认这种行；把 armor 当数据去解析会得到"找不到条目"这种假失败。
        $expected = $null
        foreach ($line in $sums) {
            if ($line -match '^\s*([0-9a-fA-F]{64})\s+\*?(\S+)\s*$') {
                if ($Matches[2] -eq $core.asset_name) { $expected = $Matches[1].ToLower() }
            }
        }
        if (-not $expected) {
            Fail "官方 SHA256SUMS 里没有 $($core.asset_name) 这一行，无法确认这是官方发布的文件。"
        }
        if ($expected -ne $actualSha) {
            Fail "官方 SHA256SUMS 记的是 $expected，实际下载到 $actualSha。两份官方资料互相矛盾，不要继续打包。"
        }
        Write-Step "官方 SHA256SUMS 交叉核对通过"
    } else {
        Write-Host "    跳过 SHA256SUMS 交叉核对（-SkipChecksumsCrossCheck）" -ForegroundColor Yellow
    }

    Write-Step "解包"
    $extractDir = Join-Path $tmpRoot "x"
    try {
        Expand-Archive -LiteralPath $zipPath -DestinationPath $extractDir -Force
    } catch {
        Fail "解包失败：$($_.Exception.Message)"
    }

    # 不假设 zip 里的子目录名，直接按文件名找，避免上游改了目录结构就整条流程失败。
    $foundExe = Get-ChildItem -LiteralPath $extractDir -Recurse -File -Filter "rclone.exe" | Select-Object -First 1
    if (-not $foundExe) { Fail "解包后没有找到 rclone.exe，这个 zip 可能不是官方 windows-amd64 包。" }

    New-Item -ItemType Directory -Path $destDir -Force | Out-Null
    Copy-Item -LiteralPath $foundExe.FullName -Destination $exePath -Force

    $licenseSource = Install-License -DestPath $copyingPath -ExtractDir $extractDir

    $exeSha = Get-Sha256 $exePath
    $exeBytes = (Get-Item -LiteralPath $exePath).Length

    # 解出来的 exe 必须与清单钉住的哈希一致：zip 的 sha256 只证明"这个压缩包是官方的"，
    # 不证明"里面那个 exe 是我们预期的那一个"（上游重打包、解包被换文件都拦不住）。
    if ($exeSha -ne $core.exe_sha256) {
        Fail "解包出来的 rclone.exe sha256 与清单不符：实际 $exeSha，清单写 $($core.exe_sha256)。内核内容变了，不要打包。"
    }
    if ($core.exe_bytes -and $exeBytes -ne [int64]$core.exe_bytes) {
        Fail "rclone.exe 大小与清单不符：实际 $exeBytes 字节，清单写 $($core.exe_bytes) 字节。"
    }

    if (-not $SkipVersionRun) {
        Write-Step "运行 rclone.exe version 确认版本"
        $versionOutput = & $exePath version 2>&1
        $versionExit = $LASTEXITCODE
        if ($versionExit -ne 0) {
            Fail "rclone.exe version 退出码 $versionExit：$versionOutput"
        }
        if (($versionOutput -join "`n") -notmatch [regex]::Escape($core.version)) {
            Fail "rclone.exe 报告的版本里没有 $($core.version)：$($versionOutput -join ' ')"
        }
        Write-Step "版本确认：$($core.version)"
    } else {
        Write-Host "    跳过版本运行检查（-SkipVersionRun）；这个 exe 没有在本机执行过" -ForegroundColor Yellow
    }

    $source = if ($LocalZip) { "local" } else { "download" }
    $marker = [ordered]@{
        version        = $core.version
        asset_name     = $core.asset_name
        asset_sha256   = $core.asset_sha256
        exe_sha256     = $exeSha
        exe_bytes      = $exeBytes
        exe_verified   = "core-manifest.json exe_sha256"
        license_sha256 = $core.license_sha256
        license_source = $licenseSource
        source         = $source
        fetched_at     = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    }
    $marker | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $markerPath -Encoding UTF8

    Write-Step "完成"
    Write-Host "    $exePath（$exeBytes 字节）"
    Write-Host "    sha256 $exeSha"
    Write-Host "    记录 $markerPath"
} finally {
    if (Test-Path -LiteralPath $tmpRoot) {
        Remove-Item -LiteralPath $tmpRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
