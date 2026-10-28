#Requires -Version 5.1
# Builds Warp Agent for Windows and packages the release exe.
#   .\build.ps1             -> release build + portable zip in out\
#   .\build.ps1 -Installer  -> also compile the Inno Setup installer (ISCC.exe on PATH)
# Run from a Windows machine — GPUI has no Linux->Windows cross build.
param(
    [switch]$Installer
)
$ErrorActionPreference = 'Stop'

$here   = $PSScriptRoot
$outDir = Join-Path $here 'out'

# repo root = closest ancestor holding Cargo.toml (script lives at <root>\packaging\windows)
$repo = $here
while ($repo -and -not (Test-Path (Join-Path $repo 'Cargo.toml'))) { $repo = Split-Path -Parent $repo }
if (-not $repo) { throw "Cargo.toml not found in any parent of $here" }

Push-Location $repo
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    $meta = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    $version = ($meta.packages | Where-Object { $_.name -eq 'warp-agent' }).version
    $exe = Join-Path $meta.target_directory 'release\warp-agent.exe'
} finally {
    Pop-Location
}

if (-not (Test-Path $exe)) { throw "release exe not found: $exe" }

New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$zip = Join-Path $outDir "warp-agent-$version-x64.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $exe -DestinationPath $zip
Write-Host "portable: $zip"

if ($Installer) {
    $iscc = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if (-not $iscc) { throw "Inno Setup compiler not found on PATH (install Inno Setup 6)" }
    & $iscc "/DMyAppVersion=$version" (Join-Path $here 'warp-agent.iss')
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed with exit code $LASTEXITCODE" }
}
