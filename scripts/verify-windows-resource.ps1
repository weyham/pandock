[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ExePath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$exe = (Resolve-Path -LiteralPath $ExePath).Path
$kitsRoot = "C:\Program Files (x86)\Windows Kits\10\bin"
$mt = Get-ChildItem -LiteralPath $kitsRoot -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending |
    ForEach-Object { Join-Path $_.FullName "x64\mt.exe" } |
    Where-Object { Test-Path -LiteralPath $_ } |
    Select-Object -First 1

if (-not $mt) {
    throw "mt.exe was not found under $kitsRoot."
}

$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("pandock-resource-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tempRoot -Force | Out-Null
$manifest = Join-Path $tempRoot "manifest.xml"
try {
    & $mt -nologo "-inputresource:$exe;#1" "-out:$manifest"
    if ($LASTEXITCODE -ne 0) { throw "mt.exe failed with exit code $LASTEXITCODE" }

    $content = [System.IO.File]::ReadAllText($manifest, [System.Text.Encoding]::UTF8)
    if ($content -notmatch 'Microsoft\.Windows\.Common\-Controls') {
        throw "The executable manifest does not declare Microsoft.Windows.Common-Controls."
    }
    if ($content -notmatch 'version="6\.0\.0\.0"') {
        throw "The executable manifest does not request Common Controls 6.0.0.0."
    }

    Write-Host "Windows resource manifest check passed."
}
finally {
    if (Test-Path -LiteralPath $tempRoot) {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force
    }
}
