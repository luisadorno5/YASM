<#
.SYNOPSIS
    Builds the Rust Node.Application.Service.Wrapper release binary and packages the Phase 2 multi-app deployment layout.
#>
[CmdletBinding()]
param(
    [string]$OutputDir = "$PSScriptRoot\Dist\service-wrapper"
)

$ErrorActionPreference = "Stop"

Write-Host "Building release binary (node-application-service-wrapper)..." -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) {
    throw "cargo build --release failed with exit code $LASTEXITCODE"
}

$WrapperOut = Join-Path $OutputDir "wrapper"
$ScriptsOut = Join-Path $OutputDir "scripts"
$AppsOut    = Join-Path $OutputDir "apps"

New-Item -ItemType Directory -Force -Path $WrapperOut | Out-Null
New-Item -ItemType Directory -Force -Path $ScriptsOut | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $AppsOut "storefront") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $AppsOut "admin") | Out-Null

$BuiltExe = Join-Path $PSScriptRoot "target\release\node-application-service-wrapper.exe"
if (-not (Test-Path $BuiltExe)) {
    $BuiltExe = Join-Path $PSScriptRoot "target\release\node-application-service-wrapper"
}

Copy-Item $BuiltExe (Join-Path $WrapperOut "Node.Application.Service.Wrapper.exe") -Force
Copy-Item (Join-Path $PSScriptRoot "appsettings.json") (Join-Path $WrapperOut "appsettings.json") -Force
Copy-Item (Join-Path $PSScriptRoot "wrapper.cmd") (Join-Path $WrapperOut "wrapper.cmd") -Force

if (Test-Path (Join-Path $PSScriptRoot "scripts")) {
    Copy-Item (Join-Path $PSScriptRoot "scripts\*.ps1") $ScriptsOut -Force
}

Write-Host "Packaged multi-app service wrapper into: $OutputDir" -ForegroundColor Green
