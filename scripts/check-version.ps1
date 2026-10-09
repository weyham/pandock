[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)

function Read-CargoWorkspaceVersion {
    $text = [System.IO.File]::ReadAllText((Join-Path $root "Cargo.toml"), $utf8NoBom)
    $match = [regex]::Match($text, '(?ms)\[workspace\.package\].*?^version\s*=\s*"([^"]+)"')
    if (-not $match.Success) { throw "Workspace version not found" }
    return $match.Groups[1].Value
}
function Read-JsonVersion([string]$Path) {
    $fullPath = Join-Path $root $Path
    $text = [System.IO.File]::ReadAllText($fullPath, $utf8NoBom)
    $match = [regex]::Match($text, '(?m)^\s*"version"\s*:\s*"([^"]+)"')
    if (-not $match.Success) { throw "Version not found in $Path" }
    return $match.Groups[1].Value
}
function Assert-Same([string]$Name, [string]$Actual, [string]$Expected) {
    if ($Actual -ne $Expected) { throw "$Name version mismatch: expected $Expected, got $Actual" }
}

$expected = Read-CargoWorkspaceVersion
Assert-Same "tauri.conf.json" (Read-JsonVersion "src-tauri\tauri.conf.json") $expected
Assert-Same "package.json" (Read-JsonVersion "package.json") $expected
Assert-Same "ui/package.json" (Read-JsonVersion "ui\package.json") $expected
Assert-Same "ui/package-lock.json" (Read-JsonVersion "ui\package-lock.json") $expected
Write-Host "All version fields match $expected."
