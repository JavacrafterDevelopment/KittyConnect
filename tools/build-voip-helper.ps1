# Builds vendor\kc-voip.exe, the VOIP rooms' TailCat helper, from voip-helper\,
# where build.rs embeds it into KittyConnect.exe. Used by build.ps1 and the
# release workflow. Needs Go (https://go.dev/dl/); the right Go version is
# fetched automatically if the installed one is older than go.mod asks for.
#
#   tools\build-voip-helper.ps1
#
# It is built on the TailCat library, the same TailCat version as the
# official tailcat.exe fetched by fetch-tailcat.ps1; this script refuses to
# build if they differ, so the two always get security updates together. It is
# built with the same Go build tags as the official binaries (TailCat's
# build-tags.txt), and it writes the licenses of everything compiled into it
# to vendor\kc-voip-licenses\.

$ErrorActionPreference = 'Stop'

$root   = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$src    = Join-Path $root 'voip-helper'
$vendor = Join-Path $root 'vendor'
$exe    = Join-Path $vendor 'kc-voip.exe'
$lic    = Join-Path $vendor 'kc-voip-licenses'

if (-not (Get-Command go -ErrorAction SilentlyContinue)) {
    throw 'Go is not installed. Get it from https://go.dev/dl/ and run this again (it builds the VOIP helper).'
}

# One TailCat version everywhere: fetch-tailcat.ps1, src\tailcat.rs and voip-helper\go.mod.
$official = (Select-String -Path (Join-Path $root 'tools\fetch-tailcat.ps1') -Pattern '^\$version\s*=\s*[''"]([^''"]+)').Matches[0].Groups[1].Value
$app      = (Select-String -Path (Join-Path $root 'src\tailcat.rs') -Pattern 'pub const VERSION: &str = "([^"]+)"').Matches[0].Groups[1].Value
$library  = (Select-String -Path (Join-Path $src 'go.mod') -Pattern '^\s*(?:require\s+)?github\.com/tailscale/tailcat\s+v(\S+)').Matches[0].Groups[1].Value
if ($official -ne $library -or $app -ne $library) {
    throw "TailCat versions differ: fetch-tailcat.ps1 has $official, src\tailcat.rs has $app, voip-helper\go.mod has $library. Update them together."
}

New-Item -ItemType Directory -Force $vendor | Out-Null
$saved = @{ GOOS = $env:GOOS; GOARCH = $env:GOARCH; CGO_ENABLED = $env:CGO_ENABLED }
Push-Location $src
try {
    $env:GOOS = 'windows'
    $env:GOARCH = 'amd64'
    $env:CGO_ENABLED = '0'

    # The modules (TailCat's source included) must be downloaded before Go can
    # say where TailCat's build tags are.
    & go mod download
    if ($LASTEXITCODE -ne 0) { throw 'go mod download failed' }
    $tcDir = (& go list -m -f '{{.Dir}}' github.com/tailscale/tailcat).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'go list failed' }
    $tags = (Get-Content (Join-Path $tcDir 'build-tags.txt') -Raw).Trim()

    & go build -tags $tags -trimpath -ldflags '-s -w' -o $exe .
    if ($LASTEXITCODE -ne 0) { throw 'go build of the VOIP helper failed' }

    # The licenses of every module compiled in, and Go's own.
    if (Test-Path $lic) { Remove-Item -Recurse -Force $lic }
    New-Item -ItemType Directory -Force $lic | Out-Null
    $mods = & go list -tags $tags -deps -f '{{with .Module}}{{if not .Main}}{{.Path}}|{{.Version}}|{{.Dir}}{{end}}{{end}}' .
    if ($LASTEXITCODE -ne 0) { throw 'go list of the VOIP helper dependencies failed' }
    $goroot = (& go env GOROOT).Trim()
    $entries = @($mods | Where-Object { $_ } | Sort-Object -Unique) + @("go|$((& go env GOVERSION).Trim())|$goroot")
    $pattern = '^(LICEN[CS]E|COPYING|NOTICE|PATENTS)(\.(txt|md))?$'
    foreach ($m in $entries) {
        $path, $ver, $dir = $m -split '\|', 3
        $files = Get-ChildItem -LiteralPath $dir -File | Where-Object { $_.Name -match $pattern }
        if (-not $files) { throw "No license file found for $path $ver in $dir" }
        $to = Join-Path $lic ($path -replace '[/\\:]', '_')
        New-Item -ItemType Directory -Force $to | Out-Null
        foreach ($f in $files) { Copy-Item $f.FullName (Join-Path $to $f.Name) }
        Set-Content -Path (Join-Path $to 'VERSION.txt') -Value "$path $ver"
    }
} finally {
    foreach ($k in $saved.Keys) { Set-Item "Env:$k" $saved[$k] -ErrorAction SilentlyContinue; if (-not $saved[$k]) { Remove-Item "Env:$k" -ErrorAction SilentlyContinue } }
    Pop-Location
}

$mb = [Math]::Round((Get-Item $exe).Length / 1MB, 1)
Write-Output "Built the VOIP helper on TailCat $library -> vendor\kc-voip.exe ($mb MB)"
