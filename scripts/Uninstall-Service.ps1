<#
.SYNOPSIS
    Stops and removes the Node.Application.Service.Wrapper Windows Service.
#>
[CmdletBinding()]
param(
    [string]$ServiceName = "NodeApplicationServiceWrapper"
)

$ErrorActionPreference = "Stop"

$Existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($null -ne $Existing) {
    if ($Existing.Status -ne 'Stopped') {
        Write-Host "Stopping service '$ServiceName'..." -ForegroundColor Yellow
        Stop-Service -Name $ServiceName -Force
    }
    & sc.exe delete $ServiceName | Out-Null
    Write-Host "Service '$ServiceName' removed." -ForegroundColor Green
} else {
    Write-Host "Service '$ServiceName' is not installed." -ForegroundColor Cyan
}
