# Builds the portable KittyConnect folder.
#
#   powershell -ExecutionPolicy Bypass -File build.ps1
#
# Produces dist\KittyConnect\ - copy that folder anywhere (a USB stick, another
# PC, a zip in Discord) and run KittyConnect.exe. Nothing is installed; TailCat
# is built into the exe, and everything it writes stays in data\ beside it.
#
# Needs Rust and Go (Go builds the VOIP rooms' helper, voip-helper\).
#
# Also writes dist\KittyConnect-windows-x64.zip.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$dist = Join-Path $root 'dist\KittyConnect'
Set-Location $root

Write-Host ''
Write-Host '  KittyConnect - portable build' -ForegroundColor DarkYellow
Write-Host '  -----------------------------' -ForegroundColor DarkGray

$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    if (Test-Path (Join-Path $cargoBin 'cargo.exe')) { $env:PATH = "$cargoBin;$env:PATH" }
    else { throw 'Rust is not installed. Get it from https://rustup.rs and run this again.' }
}

$vendor = Join-Path $root 'vendor'
Write-Host "  $(& (Join-Path $root 'tools\fetch-tailcat.ps1'))"
Write-Host "  $(& (Join-Path $root 'tools\build-voip-helper.ps1'))"

$hostTriple = ((& rustc -vV) | Select-String '^host: (.+)$').Matches[0].Groups[1].Value
$gnu = $hostTriple -like '*-windows-gnu'

if ($gnu -and -not (Get-Command dlltool -ErrorAction SilentlyContinue)) {
    # raw-dylib imports on the GNU toolchain need a dlltool; LLVM's archiver acts as one.
    Write-Host '  Setting up dlltool (rustup llvm-tools)'
    & rustup component add llvm-tools | Out-Null
    $sysroot = (& rustc --print sysroot).Trim()
    Copy-Item (Join-Path $sysroot "lib\rustlib\$hostTriple\bin\llvm-ar.exe") (Join-Path $cargoBin 'dlltool.exe') -Force
}

$exe = Join-Path $dist 'KittyConnect.exe'
if (Test-Path $exe) {
    try { [IO.File]::Open($exe, 'Open', 'ReadWrite', 'None').Close() }
    catch { throw "KittyConnect is still running from $dist. Close it and build again." }
}

Write-Host "  Compiling (release, $hostTriple)"
& cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force $dist | Out-Null
Copy-Item (Join-Path $root 'target\release\KittyConnect.exe') $dist
Copy-Item (Join-Path $vendor 'TAILCAT-LICENSE') $dist
# Everything compiled into the VOIP helper, and its licenses.
New-Item -ItemType Directory -Force (Join-Path $dist 'licenses') | Out-Null
Copy-Item -Recurse (Join-Path $vendor 'kc-voip-licenses') (Join-Path $dist 'licenses\kc-voip')

if ($gnu) {
    # GNU builds load the WebView2 loader as a DLL, so it travels with the exe.
    $registry = Join-Path $env:USERPROFILE '.cargo\registry\src'
    $ver = ((& cargo tree -p webview2-com-sys --depth 0 -e normal) | Select-Object -First 1) -replace '^webview2-com-sys v', ''
    $dll = Get-ChildItem $registry -Directory |
        ForEach-Object { Join-Path $_.FullName "webview2-com-sys-$ver\x64\WebView2Loader.dll" } |
        Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $dll) { throw "WebView2Loader.dll for webview2-com-sys $ver not found under $registry" }
    Copy-Item $dll $dist
}

$zipOut = Join-Path $root 'dist\KittyConnect-windows-x64.zip'
if (Test-Path $zipOut) { Remove-Item $zipOut }
Compress-Archive -Path $dist -DestinationPath $zipOut

$size = [Math]::Round((Get-ChildItem $dist -File | Measure-Object Length -Sum).Sum / 1MB, 1)
Write-Host "  Built KittyConnect -> $dist ($size MB)" -ForegroundColor Green
Write-Host "  Package -> $zipOut" -ForegroundColor Green
Write-Host ''
