# Сборка встроенного движка (режим 2): tunnel.dll из amneziawg-windows и wintun.dll.
# Версии и инструменты — те же, что у официального клиента AmneziaWG 3.1 (его build.bat).
# Всё скачанное проверяется по SHA-256 и лежит в engine\.deps; результат — engine\out.
# Работает в Windows PowerShell 5.1 и в pwsh 7.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue' # иначе Invoke-WebRequest в 5.1 качает в разы медленнее
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$EngineRepo = 'https://github.com/amnezia-vpn/amneziawg-windows.git'
$EngineTag = 'v3.1.20260814'
$EngineCommit = 'e90531d15802cb976773f3b63443bc281f738ca3'

$Deps = @(
    @{ Name = 'go'; Url = 'https://go.dev/dl/go1.25.14.windows-amd64.zip'
       Sha = '119044a92b3987c341cd6aebb256676dd4780d292f7b4e72a3e9976677841697' },
    @{ Name = 'llvm-mingw-20231128-ucrt-x86_64'
       Url = 'https://github.com/mstorsjo/llvm-mingw/releases/download/20231128/llvm-mingw-20231128-ucrt-x86_64.zip'
       Sha = '7a344dafa6942de2c1f4643b3eb5c5ce5317fbab671a887e4d39f326b331798f' },
    @{ Name = 'wintun'; Url = 'https://www.wintun.net/builds/wintun-0.14.1.zip'
       Sha = '07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51' }
)

$Root = $PSScriptRoot
$DepsDir = Join-Path $Root '.deps'
$SrcDir = Join-Path $Root '.src'
$OutDir = Join-Path $Root 'out'
$started = Get-Date

function Step($text) { Write-Host ("[{0,4:N0} s] {1}" -f ((Get-Date) - $started).TotalSeconds, $text) }

New-Item -ItemType Directory -Force $DepsDir, $OutDir | Out-Null

foreach ($d in $Deps) {
    if (Test-Path (Join-Path $DepsDir $d.Name)) { Step "$($d.Name): already here"; continue }
    # Архив можно положить в .deps заранее (сайт недоступен из сети сборки) — хэш проверяется так же.
    $zip = Join-Path $DepsDir "$($d.Name).zip"
    if (Test-Path $zip) {
        Step "$($d.Name): using $zip"
    } else {
        Step "$($d.Name): downloading $($d.Url)"
        Invoke-WebRequest -Uri $d.Url -OutFile $zip -UseBasicParsing
    }
    $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    if ($hash -ne $d.Sha) { Remove-Item $zip; throw "$($d.Name): SHA-256 $hash, expected $($d.Sha)" }
    Step "$($d.Name): SHA-256 ok, extracting"
    tar -xf $zip -C $DepsDir
    if ($LASTEXITCODE -ne 0) { throw "$($d.Name): tar failed" }
    Remove-Item $zip
}

if (-not (Test-Path (Join-Path $SrcDir '.git'))) {
    Step "source: cloning $EngineRepo $EngineTag"
    git -c advice.detachedHead=false clone --quiet --depth 1 --branch $EngineTag $EngineRepo $SrcDir
    if ($LASTEXITCODE -ne 0) { throw 'git clone failed' }
}
$head = (git -C $SrcDir rev-parse HEAD).Trim()
if ($head -ne $EngineCommit) { throw "source: commit $head, expected $EngineCommit ($EngineTag)" }
Step "source: $EngineTag at $head"

# Окружение сборки — как в build.cmd движка: cgo через llvm-mingw, DLL c-shared.
$env:PATH = "$DepsDir\llvm-mingw-20231128-ucrt-x86_64\bin;$DepsDir\go\bin;$env:PATH"
$env:GOROOT = "$DepsDir\go"
$env:GOPATH = "$DepsDir\gopath"
$env:GOCACHE = "$DepsDir\gocache"
$env:GOTOOLCHAIN = 'local'
$env:GOOS = 'windows'
$env:GOARCH = 'amd64'
$env:CGO_ENABLED = '1'
$env:CC = 'x86_64-w64-mingw32-gcc'
$env:CGO_CFLAGS = '-O3 -Wall -Wno-unused-function -Wno-switch -std=gnu11 -DWINVER=0x0601'
$env:CGO_LDFLAGS = '-Wl,--dynamicbase -Wl,--nxcompat -Wl,--export-all-symbols -Wl,--high-entropy-va'

Step 'tunnel.dll: go build'
Push-Location $SrcDir
try {
    go build -buildmode c-shared -ldflags='-w -s' -trimpath -o (Join-Path $OutDir 'tunnel.dll')
    if ($LASTEXITCODE -ne 0) { throw 'go build failed' }
} finally { Pop-Location }
Remove-Item (Join-Path $OutDir 'tunnel.h') -ErrorAction SilentlyContinue

Copy-Item (Join-Path $DepsDir 'wintun\bin\amd64\wintun.dll') $OutDir -Force
Set-Content -Path (Join-Path $OutDir 'ENGINE.txt') -Value "amneziawg-windows $EngineTag $EngineCommit`r`nwintun 0.14.1" -Encoding ascii

# Лицензии того, что попало в tunnel.dll (модули Go), и wintun — их требуют MIT/BSD и лицензия wintun.
Step 'licenses'
$notice = New-Object System.Text.StringBuilder
$readme = Get-Content (Join-Path $SrcDir 'README.md') -Raw
if ($readme -match '(?s)```text\r?\n(.*?)```') {
    [void]$notice.AppendLine("==== github.com/amnezia-vpn/amneziawg-windows $EngineTag ====").AppendLine($Matches[1])
}
Push-Location $SrcDir
try {
    $modules = go list -deps -f '{{with .Module}}{{.Path}}|{{.Version}}|{{.Dir}}{{end}}' . | Where-Object { $_ } | Sort-Object -Unique
} finally { Pop-Location }
foreach ($m in $modules) {
    $path, $version, $dir = $m -split '\|', 3
    if ($dir -eq $SrcDir -or -not $version) { continue }
    $license = Get-ChildItem $dir -File | Where-Object { $_.Name -match '^(LICENSE|COPYING)' } | Select-Object -First 1
    if (-not $license) { throw "no license file in module $path" }
    [void]$notice.AppendLine("==== $path $version ====").AppendLine((Get-Content $license.FullName -Raw))
}
Set-Content -Path (Join-Path $OutDir 'tunnel-LICENSES.txt') -Value $notice.ToString() -Encoding utf8
Copy-Item (Join-Path $DepsDir 'wintun\LICENSE.txt') (Join-Path $OutDir 'wintun-LICENSE.txt') -Force

Get-ChildItem $OutDir | ForEach-Object { Step ("{0}  {1:N0} bytes  {2}" -f $_.Name, $_.Length, (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower()) }
Step 'done'
