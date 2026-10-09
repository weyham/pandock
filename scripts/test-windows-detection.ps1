[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "common.ps1")
$actual = [Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT
if ($actual) {
    $saved = $env:OS
    try {
        Remove-Item Env:OS -ErrorAction SilentlyContinue
        if (-not (Test-PandockWindows)) {
            throw "Windows detection failed when OS environment variable was empty."
        }
    }
    finally {
        if ($null -ne $saved) { $env:OS = $saved }
    }
}
Write-Host "Windows detection self-test passed."
