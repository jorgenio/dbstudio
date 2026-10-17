# Build a distributable Windows zip of jorgenio dbstudio (F6 packaging).
# Usage: powershell -ExecutionPolicy Bypass -File package.ps1
param(
    [string]$Out = "dist"
)
$ErrorActionPreference = "Stop"
$env:Path += ";$env:USERPROFILE\.cargo\bin"

$meta = (cargo metadata --no-deps --format-version 1 | Out-String | ConvertFrom-Json)
$version = ($meta.packages | Where-Object { $_.name -eq "dbstudio-app" }).Version
$exe = Join-Path $meta.target_directory "release\dbstudio.exe"

cargo build --release -p dbstudio-app
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
if (!(Test-Path $exe)) { Write-Error "no se encontró $exe"; exit 1 }

New-Item -ItemType Directory -Force -Path $Out | Out-Null
$stage = Join-Path $Out "stage"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage | Out-Null

Copy-Item $exe (Join-Path $stage "dbstudio.exe")

$zip = Join-Path $Out "jorgenio-dbstudio-$version-win64.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Remove-Item $stage -Recurse -Force

Write-Host "OK: $zip ($([Math]::Round((Get-Item $zip).Length / 1MB, 1)) MB)"
