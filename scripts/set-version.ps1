[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$')]
    [string]$Version
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)

function Set-FirstVersionMatch([string]$Path, [string]$Pattern) {
    $fullPath = Join-Path $root $Path
    $text = [System.IO.File]::ReadAllText($fullPath, $utf8NoBom)
    $match = [regex]::Match($text, $Pattern)
    if (-not $match.Success) {
        throw "Version field not found in $Path"
    }
    $updated = $text.Remove($match.Index, $match.Length).Insert(
        $match.Index,
        $match.Groups[1].Value + $Version + $match.Groups[2].Value
    )
    [System.IO.File]::WriteAllText($fullPath, $updated, $utf8NoBom)
}

Push-Location $root
try {
    Set-FirstVersionMatch "Cargo.toml" '(?m)^(version\s*=\s*")[^"]+(")'
    Set-FirstVersionMatch "src-tauri\tauri.conf.json" '("version"\s*:\s*")[^"]+(")'
    Set-FirstVersionMatch "package.json" '("version"\s*:\s*")[^"]+(")'

    $uiVersion = (Get-Content -LiteralPath (Join-Path $root "ui\package.json") -Raw -Encoding UTF8 | ConvertFrom-Json).version
    if ($uiVersion -ne $Version) {
        & npm --prefix ui version $Version --no-git-tag-version
        if ($LASTEXITCODE -ne 0) { throw "npm version failed with exit code $LASTEXITCODE" }
    }

    & cargo check -p pandock-core
    if ($LASTEXITCODE -ne 0) { throw "cargo check failed with exit code $LASTEXITCODE" }

    Write-Host "Version updated to $Version."
}
finally {
    Pop-Location
}
