<#
.SYNOPSIS
校验插件商店索引 docs/store/index.json。

.DESCRIPTION
索引是公开分发的数据，RCGUI 会直接读它，所以每条都得能被机器判定。
这个脚本检查的是“结构对不对、来源是不是允许的、哈希看起来是不是真的”，
不判断插件本身好不好用——那是人工审核的事（见 docs/store/README.md）。

默认只做离线检查（不发网络请求）。加 -VerifyRemote 会真的把每个 zip 下载下来
比对 sha256，CI 里用这个模式；本机没有外网时不要加。

退出码：0 通过，1 有错误（每条错误会打印出来）。
#>
[CmdletBinding()]
param(
    [string]$IndexPath = "docs\store\index.json",
    [string]$ManifestPath = "plugins\manifest.json",
    [string]$RepoRoot = "",
    [switch]$VerifyRemote
)

$ErrorActionPreference = "Continue"

function Write-Probe([string]$Message) { Write-Host "==> $Message" }

function Resolve-RepoRoot([string]$Hint) {
    if ($Hint) {
        if (-not (Test-Path -LiteralPath $Hint)) { throw "给的 -RepoRoot 不存在：$Hint" }
        return (Resolve-Path -LiteralPath $Hint).Path
    }
    $dir = $PSScriptRoot
    if (-not $dir) { $dir = Split-Path -Parent $MyInvocation.MyCommand.Path }
    if (-not $dir) { $dir = (Get-Location).Path }
    for ($i = 0; $i -lt 6; $i++) {
        if (Test-Path -LiteralPath (Join-Path $dir "docs\store\index.json")) {
            return (Resolve-Path -LiteralPath $dir).Path
        }
        $parent = Split-Path -Parent $dir
        if (-not $parent -or $parent -eq $dir) { break }
        $dir = $parent
    }
    throw "找不到仓库根（向上找了 6 层都没看到 docs\store\index.json）。用 -RepoRoot 指定。"
}

$script:errors = New-Object System.Collections.ArrayList
function Add-Error([string]$Message) { [void]$script:errors.Add($Message) }

# 索引里的 id 会被当作目录名和 URL 段使用，所以只允许小写字母、数字和连字符。
$idPattern = '^[a-z0-9]([a-z0-9-]{0,30}[a-z0-9])?$'
$semverPattern = '^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$'
$sha256Pattern = '^[0-9a-f]{64}$'
$allowedCapabilities = @("kv", "ui", "fs", "http", "proc", "net", "rclone", "env")
# 来源：任何作者的 GitHub 公开 Release 资产都行，但 url 必须来自 repository 声明的那一个仓库。
# 只认 github.com 是为了让"谁发布的、发布在哪个仓库"能直接看出来：换域名就得人工审核，
# 脚本不放行任何非 github.com 的地址。
$repoPattern = '^https://github\.com/([A-Za-z0-9](?:[A-Za-z0-9-]{0,38}))/([A-Za-z0-9._-]+)$'
$urlPattern = '^https://github\.com/([A-Za-z0-9](?:[A-Za-z0-9-]{0,38}))/([A-Za-z0-9._-]+)/releases/download/([^/]+)/([^/]+\.zip)$'
# 明确不收录的插件：oneclick 会自己拉起 rclone 进程，和主程序管理内核的路子冲突。
# 它的 id 和目录名是 onerclone，两个写法都挡掉，免得有人改了名单又漏出来。
$blockedIds = @("onerclone", "oneclick")
$requiredFields = @("id", "name", "version", "description", "repository", "url", "sha256", "min_gui_version", "capabilities")

try {
    $root = Resolve-RepoRoot $RepoRoot
} catch {
    Write-Host "错误：$($_.Exception.Message)"
    exit 1
}

$indexFull = if ([System.IO.Path]::IsPathRooted($IndexPath)) { $IndexPath } else { Join-Path $root $IndexPath }
Write-Probe "索引文件：$indexFull"
if (-not (Test-Path -LiteralPath $indexFull)) {
    Write-Host "错误：找不到索引文件 $indexFull"
    exit 1
}

$raw = Get-Content -LiteralPath $indexFull -Raw -Encoding UTF8
try {
    $index = $raw | ConvertFrom-Json
} catch {
    Write-Host "错误：索引不是合法 JSON：$($_.Exception.Message)"
    exit 1
}

if (-not ($index.PSObject.Properties.Name -contains "schema_version")) {
    Add-Error "缺少 schema_version。"
} elseif ($index.schema_version -ne 1) {
    Add-Error "schema_version 只认 1，现在是 $($index.schema_version)。要改结构就升版本号，并同步改读取方。"
}
foreach ($key in @($index.PSObject.Properties.Name)) {
    if ($key -notin @("schema_version", "plugins")) {
        Add-Error "顶层多了不认识的字段：$key。多写的字段会被读取方忽略，写的人以为生效了，所以这里直接报错。"
    }
}

$plugins = @($index.plugins | Where-Object { $_ -ne $null })
if ($plugins.Count -eq 0) {
    Add-Error "plugins 是空的。商店可以先只有两个插件，但不能是空数组——那说明文件没填完。"
}

$seenIds = @{}
$seenUrls = @{}
foreach ($p in $plugins) {
    $label = if ($p.id) { "插件 $($p.id)" } else { "第 $([array]::IndexOf($plugins, $p) + 1) 条" }

    foreach ($f in $requiredFields) {
        if (-not ($p.PSObject.Properties.Name -contains $f)) { Add-Error "$label 缺少字段 $f。" }
    }
    foreach ($key in @($p.PSObject.Properties.Name)) {
        if ($key -notin $requiredFields) {
            Add-Error "$label 有多余字段 $key。字段少写不会出错，多写会让人以为它生效了。"
        }
    }
    if (-not $p.id) { continue }

    if ($p.id -notmatch $idPattern) {
        Add-Error "$label 的 id 只能用小写字母、数字和连字符，长度 2-32，且不能以连字符开头结尾。"
    }
    if ($p.id -in $blockedIds) {
        Add-Error "$label 在排除清单里（$($blockedIds -join ', ')）。这类插件不通过商店分发。"
    }
    if ($seenIds.ContainsKey($p.id)) {
        Add-Error "$label 的 id 重复了。同一个 id 只能有一条。"
    } else {
        $seenIds[$p.id] = $true
    }

    # name 必须是字符串：核心按字符串读，写成 {zh,en} 会让读取方拿到一个对象。
    if ($p.name -isnot [string]) {
        Add-Error "$label 的 name 必须是字符串，实际是 $($p.name.GetType().Name)。name 是给人看的名字，不是本地化对象。"
    } elseif ($p.name.Trim().Length -eq 0) {
        Add-Error "$label 的 name 是空的。"
    } elseif ($p.name.Length -gt 60) {
        Add-Error "$label 的 name 超过 60 个字符。"
    }

    if ($p.version -notmatch $semverPattern) {
        Add-Error "$label 的 version 不是语义化版本（要形如 1.2.3）：$($p.version)"
    }

    if (-not $p.description -or $p.description -isnot [string]) {
        Add-Error "$label 的 description 必须是字符串，并且能说清这个插件做什么。"
    } elseif ($p.description.Trim().Length -lt 10) {
        Add-Error "$label 的 description 太短，至少要能说清这个插件做什么。"
    } elseif ($p.description.Length -gt 300) {
        Add-Error "$label 的 description 超过 300 个字符，卡片里放不下。"
    }

    $repoMatch = $null
    if ($p.repository -notmatch $repoPattern) {
        Add-Error "$label 的 repository 必须是 https://github.com/<owner>/<repo> 形式（不带 .git、不带子路径），现在是：$($p.repository)"
    } else {
        $repoMatch = [regex]::Match($p.repository, $repoPattern)
    }

    if ($p.url -notmatch $urlPattern) {
        Add-Error "$label 的 url 必须是 https://github.com/<owner>/<repo>/releases/download/<tag>/<文件>.zip，现在是：$($p.url)"
    } else {
        $urlMatch = [regex]::Match($p.url, $urlPattern)
        # 下载地址必须和 repository 指向同一个仓库：否则商店里"看源码"和"下载"去的是两个地方。
        if ($repoMatch) {
            $urlOwner = $urlMatch.Groups[1].Value
            $urlRepo = $urlMatch.Groups[2].Value
            $repoOwner = $repoMatch.Groups[1].Value
            $repoRepo = $repoMatch.Groups[2].Value
            if ($urlOwner -ne $repoOwner -or $urlRepo -ne $repoRepo) {
                Add-Error "$label 的 url 指向 $urlOwner/$urlRepo，但 repository 是 $repoOwner/$repoRepo。下载地址必须来自声明的那一个仓库。"
            }
        }
        # 地址里不该有多余的东西：用户名的密码、查询参数、锚点、非 443 端口都会让地址不稳定或骗人。
        try {
            $uri = [System.Uri]::new($p.url)
            if ($uri.UserInfo) { Add-Error "$label 的 url 里出现了用户名或密码（userinfo）：$($p.url)" }
            if ($uri.Query) { Add-Error "$label 的 url 带了查询参数（?…）。下载地址要能长期直接用，别依赖参数。" }
            if ($uri.Fragment) { Add-Error "$label 的 url 带了片段（#…），这部分不会发给服务器。" }
            if ($uri.Scheme -ne "https") { Add-Error "$label 的 url 不是 https：$($p.url)" }
            if (($uri.Port -ne 443) -and ($uri.Port -ne -1)) { Add-Error "$label 的 url 用了端口 $($uri.Port)，只接受默认的 443。" }
        } catch {
            Add-Error "$label 的 url 解析不了：$($_.Exception.Message)"
        }

        $assetName = $urlMatch.Groups[4].Value
        $expected = "$($p.id)-$($p.version).zip"
        if ($assetName -ne $expected) {
            Add-Error "$label 的资产文件名是 $assetName，按约定应该是 $expected。文件名对不上，多半是版本号或地址抄错了。"
        }
        if ($seenUrls.ContainsKey($p.url)) { Add-Error "$label 的 url 和别的条目重复：$($p.url)" } else { $seenUrls[$p.url] = $true }
        if ($urlMatch.Groups[3].Value -notmatch '^v?\d+\.\d+\.\d+') {
            Add-Error "$label 的下载地址里看不出 tag（应该是 $($urlMatch.Groups[3].Value) 这种…），请用 releases/download/v<version>/ 形式。"
        }
    }

    if ($p.sha256 -notmatch $sha256Pattern) {
        Add-Error "$label 的 sha256 必须是 64 位小写十六进制。现在是：$($p.sha256)"
    } elseif ($p.sha256 -match '^0{64}$' -or $p.sha256 -match '^(.)\1{63}$') {
        Add-Error "$label 的 sha256 看起来是占位符（全 0 或同一个字符重复）。填占位符比留空更危险，读取方没法分辨。"
    }

    if ($p.min_gui_version -notmatch $semverPattern) {
        Add-Error "$label 的 min_gui_version 不是语义化版本：$($p.min_gui_version)"
    }

    $caps = @($p.capabilities)
    if ($caps.Count -eq 0) {
        Add-Error "$label 的 capabilities 是空的。它至少要说明要不要网络、文件、进程权限。"
    }
    foreach ($c in $caps) {
        if ($c -notin $allowedCapabilities) {
            Add-Error "$label 的 capability 不认识：$c。允许的是 $($allowedCapabilities -join ', ')。"
        }
    }
    if (($caps | Select-Object -Unique).Count -ne $caps.Count) {
        Add-Error "$label 的 capabilities 有重复项。"
    }
}

# 与 plugins/manifest.json 交叉核对：同一个插件在两处的 sha256 必须一致，
# 否则应用按清单拉的和商店页面给人看的是两个不同的包。
if ($ManifestPath) {
    $manifestFull = if ([System.IO.Path]::IsPathRooted($ManifestPath)) { $ManifestPath } else { Join-Path $root $ManifestPath }
    if (Test-Path -LiteralPath $manifestFull) {
        Write-Probe "交叉核对插件清单：$manifestFull"
        try {
            $manifest = Get-Content -LiteralPath $manifestFull -Raw -Encoding UTF8 | ConvertFrom-Json
        } catch {
            Add-Error "plugins/manifest.json 不是合法 JSON：$($_.Exception.Message)"
            $manifest = $null
        }
        if ($manifest) {
            $byId = @{}
            foreach ($mp in @($manifest.plugins)) { if ($mp.id) { $byId[$mp.id] = $mp } }
            foreach ($p in $plugins) {
                if (-not $p.id) { continue }
                if (-not $byId.ContainsKey($p.id)) {
                    Add-Error "商店里有 $($p.id)，但 plugins/manifest.json 里没有。要么清单漏了，要么这个包不是按清单发布的。"
                    continue
                }
                if ($byId[$p.id].sha256 -ne $p.sha256) {
                    Add-Error "$($p.id) 的 sha256 在两处不一致：清单 $($byId[$p.id].sha256)，商店 $($p.sha256)。"
                }
                $tag = $byId[$p.id].tag
                if ($tag -and $tag -ne "v$($p.version)") {
                    Add-Error "$($p.id) 的版本对不上：清单 tag $tag，商店 version $($p.version)。"
                }
            }
        }
    } else {
        Write-Probe "没找到插件清单 $manifestFull，跳过交叉核对。"
    }
}

# 从下载下来的 zip 里读一个 JSON 条目，读不到返回 $null。
# 只读不写，解到内存里；插件包的结构是根目录下 config.json（见 docs/PLUGIN.md）。
function Read-ZipJsonEntry {
    param([string]$ZipPath, [string]$EntryName)
    if (-not ("System.IO.Compression.ZipFile" -as [type])) {
        Add-Type -AssemblyName System.IO.Compression.FileSystem -ErrorAction Stop
    }
    $zip = [System.IO.Compression.ZipFile]::OpenRead($ZipPath)
    try {
        $entry = $zip.Entries | Where-Object { $_.FullName -eq $EntryName } | Select-Object -First 1
        if (-not $entry) { return $null }
        $reader = New-Object System.IO.StreamReader($entry.Open())
        try { return ($reader.ReadToEnd() | ConvertFrom-Json) } finally { $reader.Dispose() }
    } finally {
        $zip.Dispose()
    }
}

if ($VerifyRemote) {
    Write-Probe "下载每个包，比对 sha256 并读包里的 config.json（-VerifyRemote）"
    foreach ($p in $plugins) {
        if (-not $p.url -or $p.url -notmatch '^https://') { continue }
        $tmp = Join-Path $env:TEMP ("rcgui-store-" + [guid]::NewGuid().ToString("n") + ".zip")
        try {
            Invoke-WebRequest -Uri $p.url -OutFile $tmp -UseBasicParsing -TimeoutSec 120 | Out-Null
            $hash = (Get-FileHash -LiteralPath $tmp -Algorithm SHA256).Hash.ToLower()
            if ($hash -ne $p.sha256) {
                Add-Error "$($p.id)：下载到的包 sha256 是 $hash，索引写的是 $($p.sha256)。"
            } else {
                Write-Probe "$($p.id)：下载校验通过 $hash"
                # 哈希对得上只说明"这个 zip 没变"；包里的元数据对不对，还要拆开看。
                $cfg = Read-ZipJsonEntry -ZipPath $tmp -EntryName "config.json"
                if (-not $cfg) {
                    Add-Error "$($p.id)：包里没有根目录下的 config.json，这份 zip 不是插件包。"
                } else {
                    if ($cfg.id -ne $p.id) {
                        Add-Error "$($p.id)：包里的 config.json 写的是 id=$($cfg.id)，索引写的是 $($p.id)。"
                    }
                    if ($cfg.version -ne $p.version) {
                        Add-Error "$($p.id)：包里的 config.json 版本是 $($cfg.version)，索引写的是 $($p.version)。版本对不上说明索引指向了别的构建。"
                    }
                    if ($cfg.PSObject.Properties.Name -contains "min_gui_version" -and $cfg.min_gui_version -ne $p.min_gui_version) {
                        Add-Error "$($p.id)：包里的 min_gui_version 是 $($cfg.min_gui_version)，索引写的是 $($p.min_gui_version)。"
                    }
                    if ($cfg.PSObject.Properties.Name -contains "capabilities") {
                        $cfgCaps = @($cfg.capabilities | ForEach-Object { "$_" } | Sort-Object)
                        $idxCaps = @($p.capabilities | ForEach-Object { "$_" } | Sort-Object)
                        if (($cfgCaps -join ",") -ne ($idxCaps -join ",")) {
                            Add-Error "$($p.id)：包里的 capabilities 是 [$($cfgCaps -join ', ')]，索引写的是 [$($idxCaps -join ', ')]。商店页展示的权限必须和包实际申请的一致。"
                        }
                    }
                }
            }
        } catch {
            Add-Error "$($p.id)：下载或读取失败（$($_.Exception.Message)）。地址不可达就等于商店里有一个装不上的条目。"
        } finally {
            if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue }
        }
    }
} else {
    Write-Probe "跳过下载校验（加 -VerifyRemote 才会检查地址能否下载、哈希是否一致、包内 config.json 是否对得上）"
}

if ($script:errors.Count -gt 0) {
    Write-Host ""
    Write-Host "校验失败，共 $($script:errors.Count) 条：" -ForegroundColor Red
    $i = 0
    foreach ($e in $script:errors) { $i++; Write-Host ("  {0}. {1}" -f $i, $e) -ForegroundColor Red }
    exit 1
}

Write-Host ""
Write-Host "索引校验通过：$($plugins.Count) 个插件（$((($plugins | ForEach-Object { "$($_.id) $($_.version)" }) -join ', '))）" -ForegroundColor Green
exit 0
