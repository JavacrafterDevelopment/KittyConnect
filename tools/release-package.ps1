# Packs a built, ready-to-run app folder for the Javacrafter Launcher.
# Run by the release workflow after the build.
#
#   tools\release-package.ps1 -Folder dist\RustWinUtil -Name RustWinUtil -Exe RustWinUtil.exe `
#       -Version 0.0.1 -Out dist
#
# Writes:
#   <Folder>\launcher-manifest.json   every file in the folder with its size and SHA-256
#   <Out>\<Name>-windows-x64.zip      the folder plus that manifest: what the launcher downloads
#   <Out>\release-entry.json          this release's entry for update.json
#
# A data\ folder (the app's settings and profile) is never packed: the launcher
# never touches an installed app's data\, so settings survive every update.

param(
    [Parameter(Mandatory)] [string]$Folder,
    [Parameter(Mandatory)] [string]$Name,
    [Parameter(Mandatory)] [string]$Exe,
    [Parameter(Mandatory)] [string]$Version,
    [Parameter(Mandatory)] [string]$Out
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem

$Folder   = (Resolve-Path $Folder).Path
New-Item -ItemType Directory -Force $Out | Out-Null
$Out      = (Resolve-Path $Out).Path
$manifest = 'launcher-manifest.json'
$zipPath  = Join-Path $Out "$Name-windows-x64.zip"

if (-not (Test-Path (Join-Path $Folder $Exe))) { throw "$Exe not found in $Folder" }

$files = @(Get-ChildItem $Folder -Recurse -File |
    Where-Object { $_.FullName -notlike (Join-Path $Folder 'data\*') -and $_.Name -ne $manifest } |
    Sort-Object FullName |
    ForEach-Object {
        [ordered]@{
            path   = $_.FullName.Substring($Folder.Length + 1).Replace('\', '/')
            size   = $_.Length
            sha256 = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower()
        }
    })

$json = [ordered]@{ version = $Version; exe = $Exe; files = $files } | ConvertTo-Json -Depth 4
[IO.File]::WriteAllText((Join-Path $Folder $manifest), $json, (New-Object Text.UTF8Encoding $false))

# Built entry by entry: Windows PowerShell's Compress-Archive writes backslashes
# into nested entry names, which zip readers treat as part of the file name.
if (Test-Path $zipPath) { Remove-Item $zipPath }
$zip = [IO.Compression.ZipFile]::Open($zipPath, 'Create')
try {
    foreach ($f in @($files.path) + $manifest) {
        $src = Join-Path $Folder ($f.Replace('/', '\'))
        [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $src, $f, 'Optimal')
    }
} finally { $zip.Dispose() }

$zipInfo = Get-Item $zipPath
$entry = [ordered]@{
    version = $Version
    exe     = $Exe
    package = [ordered]@{
        name   = $zipInfo.Name
        size   = $zipInfo.Length
        sha256 = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
    }
    files   = $files
}
[IO.File]::WriteAllText((Join-Path $Out 'release-entry.json'), ($entry | ConvertTo-Json -Depth 5),
    (New-Object Text.UTF8Encoding $false))

Write-Output "Packaged $Name $Version ($($files.Count) files) -> $zipPath"
