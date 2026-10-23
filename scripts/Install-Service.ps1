<#
.SYNOPSIS
    Installs and configures the Node.Application.Service.Wrapper Windows Service.
#>
[CmdletBinding()]
param(
    [string]$ServiceName = "NodeApplicationServiceWrapper",
    [string]$DisplayName = "Node Application Service Wrapper",
    [string]$InstallRoot = "C:\Program Files\Itdg\service-wrapper",
    [string]$ServiceAccount = "NT AUTHORITY\LocalService"
)

$ErrorActionPreference = "Stop"

$WrapperDir = Join-Path $InstallRoot "wrapper"
$ExePath    = Join-Path $WrapperDir "Node.Application.Service.Wrapper.exe"
$ConfigPath = Join-Path $WrapperDir "appsettings.json"

if (-not (Test-Path $ExePath)) {
    throw "Wrapper executable not found at '$ExePath'"
}
if (-not (Test-Path $ConfigPath)) {
    throw "Configuration file not found at '$ConfigPath'"
}

$Config = Get-Content $ConfigPath -Raw | ConvertFrom-Json
$LogDir = $Config.SerilogOptions.LogDirectory
if ([string]::IsNullOrWhiteSpace($LogDir)) {
    $LogDir = Join-Path $WrapperDir "Logs"
}

New-Item -ItemType Directory -Force -Path $LogDir | Out-Null

# Grant write access on LogDirectory and WrapperDir (for state.json) to the service account
icacls $LogDir /grant "${ServiceAccount}:(OI)(CI)M" /T | Out-Null
icacls $WrapperDir /grant "${ServiceAccount}:(OI)(CI)M" /T | Out-Null

& sc.exe create $ServiceName binPath= "`"$ExePath`"" start= delayed-auto obj= $ServiceAccount DisplayName= "`"$DisplayName`""
if ($LASTEXITCODE -ne 0) {
    throw "sc.exe create failed with exit code $LASTEXITCODE"
}

& sc.exe description $ServiceName "Supervises configured Node.js and standalone applications with health checks, restart policies, and local Control API." | Out-Null
& sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/5000/restart/30000 | Out-Null
& sc.exe failureflag $ServiceName 1 | Out-Null

Write-Host "Service '$ServiceName' installed and configured successfully." -ForegroundColor Green
