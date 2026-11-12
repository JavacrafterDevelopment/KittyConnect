# Folds one release into update.json, the file the Javacrafter Launcher reads to
# find updates.
# Run by the release workflow after the GitHub release exists.
#
#   tools\publish-update.ps1 -Entry dist\release-entry.json -Repo owner/name -Tag v0.2.0 `
#       -UpdateJson path\to\update.json [-Prerelease] [-Notes text] [-Published iso-date]
#
# update.json lives on the repo's `updates` branch and is read from
# https://raw.githubusercontent.com/<repo>/updates/update.json — a CDN, with none
# of the GitHub API's 60-requests-an-hour limit. It holds one entry per channel:
#
#   stable   newest normal release
#   beta     newest release of any kind; launchers with pre-releases on read this
#
# Each entry carries the release package's URL, size and SHA-256 and the size and
# SHA-256 of every file in it. Launchers check the download against these before
# installing anything.

param(
    [Parameter(Mandatory)] [string]$Entry,
    [Parameter(Mandatory)] [string]$Repo,
    [Parameter(Mandatory)] [string]$Tag,
    [Parameter(Mandatory)] [string]$UpdateJson,
    [switch]$Prerelease,
    [string]$Notes = '',
    [string]$Published = ''
)

$ErrorActionPreference = 'Stop'

$release = Get-Content $Entry -Raw | ConvertFrom-Json
$package = $release.package

$item = [ordered]@{
    version   = $release.version
    published = $(if ($Published) { $Published } else { (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ') })
    notes     = $Notes
    page      = "https://github.com/$Repo/releases/tag/$Tag"
    package   = [ordered]@{
        url    = "https://github.com/$Repo/releases/download/$Tag/$($package.name)"
        size   = $package.size
        sha256 = $package.sha256
    }
    exe       = $release.exe
    files     = @($release.files)
}

# Keep whatever the other channel already says.
$channels = [ordered]@{}
if (Test-Path $UpdateJson) {
    $existing = Get-Content $UpdateJson -Raw | ConvertFrom-Json
    foreach ($p in $existing.channels.PSObject.Properties) { $channels[$p.Name] = $p.Value }
}

$channels['beta'] = $item
if (-not $Prerelease) { $channels['stable'] = $item }

$doc = [ordered]@{ schema = 1; channels = $channels }
[IO.File]::WriteAllText($UpdateJson, ($doc | ConvertTo-Json -Depth 8), (New-Object Text.UTF8Encoding $false))
Write-Output "update.json: $Tag -> $(if ($Prerelease) { 'beta' } else { 'stable + beta' })"
