# Checks whether the engine sources and tools pinned in engine\build.ps1 are still the newest upstream.
#
# Compared: amneziawg-windows tag, wintun, amneziawg-go (the version required by go.mod at our pin vs at the latest
# tag) and the Go toolchain (latest patch of the pinned minor line). llvm-mingw only with -IncludeLlvmMingw: its pin
# deliberately mirrors the official client's build, so a newer release is not a reason for an issue by default.
#
# stdout: one JSON object {status, items:[{name, ours, latest, url}], error}; progress and errors go to stderr.
# Exit code: 0 = everything is fresh, 10 = something is outdated, 2 = the check itself failed (network, rate limit,
# unexpected page or file format). A failed check never looks like "fresh".
#
# The -Pin* parameters override a pin read from build.ps1 (to test the "outdated" path); the -*Url parameters point
# the check at another server (to test the "failed" path). GH_TOKEN / GITHUB_TOKEN, if set, is sent to the GitHub
# API only. Works in Windows PowerShell 5.1 and in pwsh 7.

[CmdletBinding()]
param(
    [string]$BuildScript,
    [string]$PinEngineTag,
    [string]$PinEngineCommit,
    [string]$PinWintun,
    [string]$PinGo,
    [string]$PinLlvmMingw,
    [switch]$IncludeLlvmMingw,
    [string]$GitHubApiUrl = 'https://api.github.com',
    [string]$GitHubRawUrl = 'https://raw.githubusercontent.com',
    [string]$WintunSiteUrl = 'https://www.wintun.net/',
    [string]$GoDownloadsUrl = 'https://go.dev/dl/'
)

$ErrorActionPreference = 'Stop'
# $PSScriptRoot is empty in a param() default under Windows PowerShell 5.1, so the default is set here.
if (-not $BuildScript) { $BuildScript = Join-Path $PSScriptRoot 'build.ps1' }
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$EXIT_FRESH = 0
$EXIT_OUTDATED = 10
$EXIT_FAILED = 2

function Say($text) { [Console]::Error.WriteLine("check-upstream: $text") }

function Get-Http([string]$Url) {
    $headers = @{ 'User-Agent' = 'awg-ui-check-upstream' }
    $token = if ($env:GH_TOKEN) { $env:GH_TOKEN } else { $env:GITHUB_TOKEN }
    # The token goes only to the GitHub API, never to the other hosts we read.
    if ($token -and $Url.StartsWith($GitHubApiUrl)) { $headers['Authorization'] = "Bearer $token" }
    try {
        $r = Invoke-WebRequest -Uri $Url -Headers $headers -UseBasicParsing -TimeoutSec 30
    } catch {
        $status = ''
        if ($_.Exception.Response) { $status = " (HTTP $([int]$_.Exception.Response.StatusCode))" }
        throw "GET $Url failed$status`: $($_.Exception.Message)"
    }
    if ($r.Content -is [byte[]]) { return [Text.Encoding]::UTF8.GetString($r.Content) }
    return [string]$r.Content
}

function Get-Json([string]$Url) {
    $text = Get-Http $Url
    try { return ConvertFrom-Json $text } catch { throw "GET $Url returned invalid JSON: $($_.Exception.Message)" }
}

# ConvertFrom-Json in Windows PowerShell 5.1 returns a JSON array as one object; this always yields a flat list.
function Get-JsonList([string]$Url) {
    $parsed = Get-Json $Url
    return ,@($parsed | ForEach-Object { $_ })
}

# "v3.1.20260814" -> 3,1,20260814; a "-suffix" (pre-release, pseudo-version) is cut off and handled by Compare-Version.
function ConvertTo-VersionParts([string]$Version) {
    $core = ($Version -replace '^[vV]', '') -replace '[-+].*$', ''
    if ($core -notmatch '^\d+(\.\d+)*$') { throw "not a version: '$Version'" }
    return @($core.Split('.') | ForEach-Object { [long]$_ })
}

# -1 / 0 / 1. Equal numbers: the one with a pre-release suffix is older.
function Compare-Version([string]$A, [string]$B) {
    $pa = ConvertTo-VersionParts $A
    $pb = ConvertTo-VersionParts $B
    $n = [Math]::Max($pa.Count, $pb.Count)
    for ($i = 0; $i -lt $n; $i++) {
        $x = if ($i -lt $pa.Count) { $pa[$i] } else { 0 }
        $y = if ($i -lt $pb.Count) { $pb[$i] } else { 0 }
        if ($x -ne $y) { return [Math]::Sign($x - $y) }
    }
    $sa = $A -match '-'
    $sb = $B -match '-'
    if ($sa -and -not $sb) { return -1 }
    if ($sb -and -not $sa) { return 1 }
    return 0
}

function Get-PinFromBuildScript([string]$Text, [string]$Pattern, [string]$What) {
    $m = [regex]::Match($Text, $Pattern)
    if (-not $m.Success) { throw "cannot find $What in $BuildScript (pattern $Pattern)" }
    return $m.Groups[1].Value
}

# Latest stable amneziawg-windows tag: pre-release tags ("-rc1") and tags of releases marked pre-release/draft are skipped.
function Get-LatestEngineTag {
    $repo = "$GitHubApiUrl/repos/amnezia-vpn/amneziawg-windows"
    $skip = @{}
    foreach ($rel in (Get-JsonList "$repo/releases?per_page=100")) {
        if ($rel.prerelease -or $rel.draft) { $skip[$rel.tag_name] = $true }
    }
    $best = $null
    for ($page = 1; $page -le 10; $page++) {
        $tags = (Get-JsonList "$repo/tags?per_page=100&page=$page")
        foreach ($t in $tags) {
            if ($t.name -notmatch '^v\d+(\.\d+)*$' -or $skip.ContainsKey($t.name)) { continue }
            if (-not $best -or (Compare-Version $t.name $best) -gt 0) { $best = $t.name }
        }
        if ($tags.Count -lt 100) { break }
    }
    if (-not $best) { throw "no stable tag found in $repo" }
    return $best
}

function Get-LatestWintun {
    $page = Get-Http $WintunSiteUrl
    $versions = @([regex]::Matches($page, 'wintun-(\d+(?:\.\d+)+)\.zip') | ForEach-Object { $_.Groups[1].Value } | Select-Object -Unique)
    if (-not $versions) { throw "no wintun-X.Y.Z.zip link on $WintunSiteUrl" }
    $best = $versions[0]
    foreach ($v in $versions) { if ((Compare-Version $v $best) -gt 0) { $best = $v } }
    return $best
}

# Latest patch of the pinned Go minor line (a new minor is a deliberate migration, not an automatic bump).
function Get-LatestGoPatch([string]$Pinned) {
    $minor = ($Pinned -split '\.')[0..1] -join '.'
    $releases = Get-JsonList "$GoDownloadsUrl`?mode=json&include=all"
    $best = $null
    foreach ($r in $releases) {
        if (-not $r.stable -or $r.version -notmatch '^go(\d+\.\d+(?:\.\d+)?)$') { continue }
        $v = $Matches[1]
        if ((($v -split '\.')[0..1] -join '.') -ne $minor) { continue }
        if (-not $best -or (Compare-Version $v $best) -gt 0) { $best = $v }
    }
    if (-not $best) { throw "no stable Go $minor release in $GoDownloadsUrl" }
    return $best
}

function Get-LatestLlvmMingw {
    $rel = Get-Json "$GitHubApiUrl/repos/mstorsjo/llvm-mingw/releases/latest"
    if (-not $rel.tag_name) { throw 'llvm-mingw: no tag_name in the latest release' }
    return [string]$rel.tag_name
}

# Version of amneziawg-go that amneziawg-windows asks for in its go.mod at the given ref (tag or commit).
function Get-GoModuleVersion([string]$Ref) {
    $url = "$GitHubRawUrl/amnezia-vpn/amneziawg-windows/$Ref/go.mod"
    $m = [regex]::Match((Get-Http $url), 'github\.com/amnezia-vpn/amneziawg-go(?:/v\d+)?\s+(v\S+)')
    if (-not $m.Success) { throw "no amneziawg-go requirement in $url" }
    return $m.Groups[1].Value
}

function Get-Outdated {
    $build = Get-Content -LiteralPath $BuildScript -Raw
    $tag = if ($PinEngineTag) { $PinEngineTag } else { Get-PinFromBuildScript $build '\$EngineTag\s*=\s*''([^'']+)''' '$EngineTag' }
    # go.mod is read at the pinned commit (immutable); with an overridden tag and no commit - at the tag.
    $ref = if ($PinEngineCommit) { $PinEngineCommit } elseif ($PinEngineTag) { $PinEngineTag }
           else { Get-PinFromBuildScript $build '\$EngineCommit\s*=\s*''([0-9a-f]{40})''' '$EngineCommit' }
    $wintun = if ($PinWintun) { $PinWintun } else { Get-PinFromBuildScript $build 'wintun-(\d+(?:\.\d+)+)\.zip' 'wintun URL' }
    $go = if ($PinGo) { $PinGo } else { Get-PinFromBuildScript $build 'dl/go(\d+\.\d+(?:\.\d+)?)\.windows-amd64\.zip' 'Go URL' }
    Say "pins: amneziawg-windows $tag ($ref), wintun $wintun, go $go"

    $items = New-Object System.Collections.Generic.List[object]
    function Add-IfOutdated($Name, $Ours, $Latest, $Url) {
        Say ("{0}: ours {1}, latest {2}" -f $Name, $Ours, $Latest)
        if ((Compare-Version $Latest $Ours) -gt 0) {
            $items.Add([pscustomobject]@{ name = $Name; ours = $Ours; latest = $Latest; url = $Url })
        }
    }

    $latestTag = Get-LatestEngineTag
    Add-IfOutdated 'amneziawg-windows' $tag $latestTag "https://github.com/amnezia-vpn/amneziawg-windows/tree/$latestTag"

    # Latest tag's go.mod is read even when our pin is the latest: both sides are then the same file and match.
    $goOurs = Get-GoModuleVersion $ref
    $goLatest = Get-GoModuleVersion $latestTag
    Add-IfOutdated 'amneziawg-go' $goOurs $goLatest "https://github.com/amnezia-vpn/amneziawg-go/tree/$goLatest"

    Add-IfOutdated 'wintun' $wintun (Get-LatestWintun) 'https://www.wintun.net/'
    Add-IfOutdated 'go' $go (Get-LatestGoPatch $go) 'https://go.dev/dl/'

    if ($IncludeLlvmMingw) {
        $mingw = if ($PinLlvmMingw) { $PinLlvmMingw } else { Get-PinFromBuildScript $build 'llvm-mingw/releases/download/(\d+)/' 'llvm-mingw URL' }
        $latestMingw = Get-LatestLlvmMingw
        Add-IfOutdated 'llvm-mingw' $mingw $latestMingw "https://github.com/mstorsjo/llvm-mingw/releases/tag/$latestMingw"
    }
    return ,$items.ToArray()
}

function Write-Result([string]$Status, $Items, [string]$Err) {
    $result = [ordered]@{ status = $Status; items = @($Items); error = $Err }
    # -InputObject keeps a one-element array an array in Windows PowerShell 5.1.
    ConvertTo-Json -InputObject $result -Depth 4
}

try {
    $outdated = Get-Outdated
} catch {
    Say "FAILED: $($_.Exception.Message)"
    Write-Result 'failed' @() $_.Exception.Message
    exit $EXIT_FAILED
}

if ($outdated.Count -gt 0) {
    Say "OUTDATED: $(($outdated | ForEach-Object { $_.name }) -join ', ')"
    Write-Result 'outdated' $outdated ''
    exit $EXIT_OUTDATED
}
Say 'fresh'
Write-Result 'fresh' @() ''
exit $EXIT_FRESH
