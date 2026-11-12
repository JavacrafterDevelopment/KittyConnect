# Puts the official TailCat Windows binary in vendor\, where build.rs embeds it
# into KittyConnect.exe. Used by build.ps1 and the release workflow.
#
#   tools\fetch-tailcat.ps1
#
# The download is checked against a pinned SHA-256 before anything is kept.
# To move to a newer TailCat, change the version and hash below (the hash is in
# that release's checksums.txt).

$ErrorActionPreference = 'Stop'

$version = '0.7.0'
$sha256  = 'f04cac07e3bf6c700b0b543df0d3315a3b312cd5a05ee76714e1cc848e9b2dff'
$zipName = "tailcat_${version}_windows_amd64.zip"

$root   = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$vendor = Join-Path $root 'vendor'
$exe    = Join-Path $vendor 'tailcat.exe'
$stamp  = Join-Path $vendor 'tailcat.version'

if ((Test-Path $exe) -and (Test-Path $stamp) -and ((Get-Content $stamp -Raw).Trim() -eq $version)) {
    Write-Output "TailCat $version already in vendor\"
    return
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("kc-tailcat-" + [guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
    $zip = Join-Path $tmp $zipName
    Invoke-WebRequest "https://github.com/tailscale/tailcat/releases/download/v$version/$zipName" -OutFile $zip -UseBasicParsing
    $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    if ($hash -ne $sha256) { throw "TailCat download hash mismatch ($hash). Not using it." }
    Expand-Archive $zip (Join-Path $tmp 'x')
    New-Item -ItemType Directory -Force $vendor | Out-Null
    Copy-Item (Join-Path $tmp 'x\tailcat.exe') $exe -Force
    Copy-Item (Join-Path $tmp 'x\LICENSE') (Join-Path $vendor 'TAILCAT-LICENSE') -Force
    Set-Content $stamp $version -NoNewline
    Write-Output "Fetched TailCat $version (sha256 verified)"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
