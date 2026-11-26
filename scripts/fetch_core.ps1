<#
.SYNOPSIS
    按 packaging/core-manifest.json 取固定版本、固定哈希的 rclone 内核，校验后放到 core\ 目录。

.DESCRIPTION
    这个脚本只做一件事：拿一个说得清来路的 rclone.exe。

    清单里有两个可互换的内核来源（sources），默认用 default_source：
      extra    gulp79/rclone-extra 的增强版（多出 alist、alldebrid、teldrive 这些后端）
      upstream rclone 官方原版
    -Source 选另一个来源；不管选哪个，校验路径完全一样。

    校验顺序（任何一步失败都停下，不会降级成警告）：
      1. 清单里该来源的 asset_sha256 / exe_sha256 / license_sha256 必须是 64 位十六进制，
         且 asset_url / license_url 落在该来源自己声明的仓库前缀下；
      2. 下载到的 zip 字节数和 sha256 必须与清单一致；
      3. 再拿该来源的 SHA256SUMS 交叉核对同一个文件（PGP 明文签名，只匹配
         "<sha256>  <文件名>" 这种行，不理会 armor 那层）。增强版没有发布 SHA256SUMS，
         清单里 checksums_url 是 null，这一步跳过并在 .fetched.json 里记下原因；
      4. 解包出来的 rclone.exe 自己的 sha256（和字节数）必须等于清单的
         exe_sha256 / exe_bytes —— zip 的哈希只说明"压缩包是那个"，
         说明不了"里面那个 exe 就是我预期的那个"；
      5. 真的运行一次 `rclone.exe version`，确认版本与清单的 exe_version_output 一致
         （原版是 rclone v1.76.0，增强版是 rclone v1.76.0-extra，这样才不会两者互串）；
      6. 许可证全文要与清单的 license_sha256 一致，否则算失败。

    第 6 步不能省的原因：两个来源的 windows zip 里都没有 COPYING（原版 v1.76.0 实测只有
    README.txt、rclone.exe、git-log.txt、rclone.1、README.html；增强版的 zip 里只有
    rclone.exe 一个文件），而发布包里必须有 rclone 的许可证全文。所以清单给每个来源
    钉了 license_url（指向各自的 version tag）与 license_sha256，许可证按它单独取、
    单独校验。

    想用本地已有的内核包，传 -LocalZip：仍然走第 1–6 步，只是省掉下载。
    这样 CI 与本地打包用的是同一条校验路径。
    许可证也可以一起用 -LicenseFile 指到已经下好的 COPYING，同样过哈希校验。

    只想检查清单本身写得对不对（不联网、不下载），传 -CheckManifest：CI 用它，
    一个 30 MB 的下载都不用发生。

.PARAMETER Dest
    内核输出目录，默认 target\core（相对仓库根）。脚本会写 rclone.exe、COPYING、.fetched.json。
    默认不放在仓库根：以前默认是 core，忘了传 -Dest 就会往仓库根丢一个 85 MB 的
    rclone.exe，而 .gitignore 并没有忽略它，`git add -A` 会把它带进提交。

.PARAMETER Manifest
    固定版本清单，默认 packaging/core-manifest.json。

.PARAMETER Source
    内核来源的名字，必须是清单 sources 里的一把钥匙，默认取清单的 default_source。
    传错名字会立刻报错并列出可选值，不会退回到默认来源。

.PARAMETER LocalZip
    已有内核 zip 的路径。仍然校验 sha256，不接受"信任本地文件"。

.PARAMETER LicenseFile
    已有的 rclone 许可证全文（COPYING）路径。给离线环境用：清单里的 license_url 下载不到时，
    用本地这份代替，但大小与 sha256 仍然按清单核对。正式发布包不要依赖它，走清单地址更可追溯。

.PARAMETER CheckManifest
    只校验清单结构（schema、来源列表、哈希位数、URL 前缀），打印每个来源的摘要后退出。
    不联网、不写任何文件。

.PARAMETER Force
    即使目标目录已有校验通过的内核，也重新取一次。

.PARAMETER SkipChecksumsCrossCheck
    跳过 SHA256SUMS 交叉核对，只在离线环境下用。manifest 里的 sha256
    依然必需匹配——这个开关不会让未校验的文件过关。

.PARAMETER SkipVersionRun
    不运行 `rclone.exe version`（例如在非 x64 主机上准备产物）。

.EXAMPLE
    .\scripts\fetch_core.ps1
    默认来源（增强版）落在 target\core\：rclone.exe、COPYING、.fetched.json 三件。

.EXAMPLE
    .\scripts\fetch_core.ps1 -Source upstream -Dest target\core-upstream
    取官方原版内核，放到另一个目录，两个内核可以并存。

.EXAMPLE
    .\scripts\fetch_core.ps1 -CheckManifest
    只检查清单，不联网。

.NOTES
    退出码：0 成功；1 校验失败或前置条件不满足。
#>
[CmdletBinding()]
param(
    [string]$Dest = "target\core",
    [string]$Manifest = "packaging/core-manifest.json",
    [string]$Source = "",
    [string]$LocalZip = "",
    [string]$LicenseFile = "",
    [switch]$CheckManifest,
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

# schema 2 起清单里有多个来源，schema 1 只有 core 一段。
# 直接拒绝旧格式，而不是猜：猜错的代价是打出一个来路不明的内核。
if ($parsed.schema_version -ne 2) {
    Fail "manifest 的 schema_version 应为 2，实际是 $($parsed.schema_version)。schema 1 的清单只有一个 core 段，不再支持。"
}
$sources = $parsed.sources
if (-not $sources) { Fail "manifest 里没有 sources 段。" }
$sourceKeys = @($sources.PSObject.Properties.Name)
if ($sourceKeys.Count -eq 0) { Fail "manifest 的 sources 是空的。" }

$defaultSource = $parsed.default_source
if (-not $defaultSource) { Fail "manifest 缺少 default_source（说明默认打哪个来源）。" }
if ($sourceKeys -notcontains $defaultSource) {
    Fail "manifest 的 default_source 是 '$defaultSource'，但 sources 里只有：$($sourceKeys -join ', ')。"
}

# 每个来源逐字段校验。单独抽成函数是为了 -CheckManifest 能把所有来源都过一遍，
# 而不是只验当前选中的那个——清单里躺着的那把钥匙写错了，同样是发布事故。
function Assert-SourceFields {
    param([string]$Key)

    $s = $sources.PSObject.Properties[$Key].Value
    if (-not $s) { Fail "manifest 的 sources.$Key 是空的。" }

    foreach ($field in @("version", "asset_name", "asset_url", "asset_sha256", "license_url", "license_sha256", "exe_sha256", "asset_url_prefix", "license_url_prefix")) {
        if (-not $s.$field) { Fail "manifest 缺少字段 sources.$Key.$field" }
    }
    if ($s.asset_sha256 -notmatch '^[0-9a-f]{64}$') {
        Fail "manifest 的 sources.$Key.asset_sha256 不是 64 位小写十六进制：'$($s.asset_sha256)'。请填 Release 资产页上的值，不要写占位符。"
    }
    if ($s.license_sha256 -notmatch '^[0-9a-f]{64}$') {
        Fail "manifest 的 sources.$Key.license_sha256 不是 64 位小写十六进制：'$($s.license_sha256)'。"
    }
    # exe_sha256 是解包后那个 rclone.exe 的哈希，不是 zip 的哈希。
    # 打包脚本会拿它核对实际文件，所以它必须是人工从可信来源量出来的值，
    # 不能由这个脚本"下载完算一遍再写进去"——那样等于自己给自己发证书。
    if ($s.exe_sha256 -notmatch '^[0-9a-f]{64}$') {
        Fail "manifest 的 sources.$Key.exe_sha256 不是 64 位小写十六进制：'$($s.exe_sha256)'。"
    }

    # 前缀由清单自己声明，脚本不再写死 rclone 官方仓库。
    # 这样加一个来源只改清单，但"URL 指向别处"这件事仍然会被拦下。
    if (-not $s.asset_url.StartsWith($s.asset_url_prefix)) {
        Fail "sources.$Key.asset_url 不在它自己声明的仓库前缀下：$($s.asset_url)（前缀 $($s.asset_url_prefix)）。内核包必须来自这个来源的 Release 资产。"
    }
    if (-not $s.license_url.StartsWith($s.license_url_prefix)) {
        Fail "sources.$Key.license_url 不在它自己声明的仓库前缀下：$($s.license_url)（前缀 $($s.license_url_prefix)）。"
    }
    # 许可证必须钉在版本 tag 上：指向 master / latest 拿到的可能是别的版本的文本。
    if ($s.license_url -notmatch [regex]::Escape("/$($s.version)/")) {
        Fail "sources.$Key.license_url 没有钉在 $($s.version) 这个 tag 上：$($s.license_url)。指向 master 或 latest 拿到的许可证可能不是这个内核的版本。"
    }
    # checksums_url 允许为 null（增强版没有发布 SHA256SUMS），但写了就必须是 https。
    if ($s.checksums_url -and -not $s.checksums_url.StartsWith("https://")) {
        Fail "sources.$Key.checksums_url 不是 https 地址：$($s.checksums_url)"
    }
}

foreach ($key in $sourceKeys) { Assert-SourceFields -Key $key }

if ($CheckManifest) {
    Write-Step "清单 $Manifest（schema 2），默认来源 $defaultSource"
    foreach ($key in $sourceKeys) {
        $s = $sources.PSObject.Properties[$key].Value
        $sums = if ($s.checksums_url) { "有官方 SHA256SUMS" } else { "无官方 SHA256SUMS（只靠清单哈希）" }
        $mark = if ($key -eq $defaultSource) { " ←默认" } else { "" }
        Write-Host ("    {0,-9} {1} {2}  zip {3}… / exe {4}…  {5}{6}" -f `
                $key, $s.project, $s.version, $s.asset_sha256.Substring(0, 12), $s.exe_sha256.Substring(0, 12), $sums, $mark)
    }
    Write-Step "清单结构无误（没有联网，也没有下载任何东西）"
    exit 0
}

# 名字写错就报错并列出可选值，不要悄悄退回默认来源——
# 否则一次手误就会让"我想打原版"变成"又打了一遍增强版"。
$sourceKey = if ($Source) { $Source } else { $defaultSource }
if ($sourceKeys -notcontains $sourceKey) {
    Fail "没有名为 '$sourceKey' 的内核来源。可选：$($sourceKeys -join ', ')。"
}
$core = $sources.PSObject.Properties[$sourceKey].Value

$versionExpect = if ($core.exe_version_output) { $core.exe_version_output } else { $core.version }

$destDir = if ([System.IO.Path]::IsPathRooted($Dest)) { $Dest } else { Join-Path $repoRoot $Dest }
$exePath = Join-Path $destDir "rclone.exe"
$copyingPath = Join-Path $destDir "COPYING"
$markerPath = Join-Path $destDir ".fetched.json"

# 许可证全文单独取：先看 zip 里有没有（将来上游可能补上），哈希对就用包内的；
# 否则按清单钉住的地址下载。大小、哈希任何一项不符都 Fail ——
# 正式发布包必须带许可证全文，这件事没有"警告一下继续"的档位。
# 读 $core 是脚本作用域里当前来源的清单值，调用方不用再传一遍。
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

    if ($LicenseFile) {
        $localPath = if ([System.IO.Path]::IsPathRooted($LicenseFile)) { $LicenseFile } else { Join-Path $repoRoot $LicenseFile }
        if (-not (Test-Path -LiteralPath $localPath)) {
            Fail "找不到 -LicenseFile 指定的文件：$localPath。"
        }
        $localBytes = (Get-Item -LiteralPath $localPath).Length
        if ($core.license_bytes -and $localBytes -ne [int64]$core.license_bytes) {
            Fail "许可证大小与清单不符：$localPath 是 $localBytes 字节，清单写 $($core.license_bytes) 字节。"
        }
        $localSha = Get-Sha256 $localPath
        if ($localSha -ne $core.license_sha256) {
            Fail "许可证 sha256 与清单不符：$localPath 是 $localSha，清单写 $($core.license_sha256)。"
        }
        Copy-Item -LiteralPath $localPath -Destination $DestPath -Force
        Write-Host "    许可证取自 -LicenseFile $localPath（$localSha）" -ForegroundColor Yellow
        return "local-file"
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

Write-Step "内核来源 $sourceKey（$($core.project) $($core.version)，$($core.platform)-$($core.arch)），目标目录 $destDir"

if ((Test-Path -LiteralPath $exePath) -and -not $Force) {
    $markerOk = $false
    if (Test-Path -LiteralPath $markerPath) {
        try {
            $marker = Get-Content -LiteralPath $markerPath -Raw -Encoding UTF8 | ConvertFrom-Json
            # 拿清单里钉住的哈希核对磁盘上的文件本身。marker 里的 exe_sha256
            # 只是随行记录，旧版本写的是"下载时现算的值"，不能反过来当依据。
            # 来源也要对上：同一个目录里原本可能是另一个来源的内核。
            if ($marker.source_key -eq $sourceKey -and $marker.version -eq $core.version -and (Get-Sha256 $exePath) -eq $core.exe_sha256) {
                $markerOk = $true
            } elseif ($marker.source_key -and $marker.source_key -ne $sourceKey) {
                Write-Step "目标目录里是 $($marker.source_key) 的内核，要换成 $sourceKey，重新取一次"
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
        Fail "内核包大小与清单不符：实际 $zipBytes 字节，清单写 $($core.asset_bytes) 字节。"
    }

    $actualSha = Get-Sha256 $zipPath
    if ($actualSha -ne $core.asset_sha256) {
        Fail "内核包 sha256 与清单不符：实际 $actualSha，清单写 $($core.asset_sha256)。"
    }
    Write-Step "zip sha256 与清单一致：$actualSha"

    # 交叉核对的结论要写进 .fetched.json：打包脚本和发布说明都靠它判断
    # "这个内核到底被两份独立资料确认过，还是只有清单一份"。
    $crossCheck = ""
    if ($SkipChecksumsCrossCheck) {
        $crossCheck = "skipped-by-flag"
        Write-Host "    跳过 SHA256SUMS 交叉核对（-SkipChecksumsCrossCheck）" -ForegroundColor Yellow
    } elseif (-not $core.checksums_url) {
        $crossCheck = "no-official-sums-published"
        Write-Host "    该来源没有发布 SHA256SUMS（清单 checksums_url 为 null），交叉核对无从做起；可信依据只有清单钉住的 asset_sha256" -ForegroundColor Yellow
    } else {
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
        $crossCheck = "verified"
        Write-Step "官方 SHA256SUMS 交叉核对通过"
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
    if (-not $foundExe) { Fail "解包后没有找到 rclone.exe，这个 zip 可能不是 windows-amd64 包。" }

    New-Item -ItemType Directory -Path $destDir -Force | Out-Null
    Copy-Item -LiteralPath $foundExe.FullName -Destination $exePath -Force

    $licenseSource = Install-License -DestPath $copyingPath -ExtractDir $extractDir

    $exeSha = Get-Sha256 $exePath
    $exeBytes = (Get-Item -LiteralPath $exePath).Length

    # 解出来的 exe 必须与清单钉住的哈希一致：zip 的 sha256 只证明"这个压缩包是那个"，
    # 不证明"里面那个 exe 是我们预期的那一个"（重打包、解包被换文件都拦不住）。
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
        # 比对完整版本串而不是 version 字段本身：增强版报的是 "rclone v1.76.0-extra"，
        # 拿 "v1.76.0" 去 match 会连原版 exe 一起放过。
        if (($versionOutput -join "`n") -notmatch [regex]::Escape($versionExpect)) {
            Fail "rclone.exe 报告的版本里没有 '$versionExpect'：$($versionOutput -join ' ')。这个 exe 不是 $sourceKey 来源的内核。"
        }
        Write-Step "版本确认：$versionExpect"
    } else {
        Write-Host "    跳过版本运行检查（-SkipVersionRun）；这个 exe 没有在本机执行过" -ForegroundColor Yellow
    }

    $source = if ($LocalZip) { "local" } else { "download" }
    $marker = [ordered]@{
        source_key      = $sourceKey
        project         = $core.project
        version         = $core.version
        version_output  = $versionExpect
        asset_name      = $core.asset_name
        asset_sha256    = $core.asset_sha256
        asset_digest    = $core.asset_digest_api
        checksums_check = $crossCheck
        exe_sha256      = $exeSha
        exe_bytes       = $exeBytes
        exe_verified    = "core-manifest.json sources.$sourceKey.exe_sha256"
        license_url     = $core.license_url
        license_sha256  = $core.license_sha256
        license_source  = $licenseSource
        source          = $source
        fetched_at      = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    }
    $marker | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $markerPath -Encoding UTF8

    Write-Step "完成（来源 $sourceKey）"
    Write-Host "    $exePath（$exeBytes 字节）"
    Write-Host "    sha256 $exeSha"
    Write-Host "    记录 $markerPath"
} finally {
    if (Test-Path -LiteralPath $tmpRoot) {
        Remove-Item -LiteralPath $tmpRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# 成功也必须显式 exit 0。调用方（build-release.ps1 的 Invoke-Native、GitHub 的 pwsh 壳）
# 只看退出码；脚本自然结束时 PowerShell 不会设置它，读到的要么是上一条原生命令留下的
# 旧值，要么干脆不存在——两种情况都会把这次成功报成失败。
exit 0
