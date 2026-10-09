# Regenerate the full Pandock icon set from src-tauri/icons/pandock.svg.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/make-icons.ps1
#
# Produces the shim-compatible core set:
#   32x32.png  128x128.png  128x128@2x.png  icon.png(256)  icon.ico  icon.icns
#   status-green.png  status-yellow.png  status-red.png
# The 16/20/24/48/64 PNGs are build-time intermediates for icon.ico / icon.icns
# and are removed afterwards (re-run this script to recreate them).
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$svg = Join-Path $root 'src-tauri\icons\pandock.svg'
$iconDir = Join-Path $root 'src-tauri\icons'
$render = Join-Path $PSScriptRoot 'render-svg.ps1'
$intermediates = @(16, 20, 24, 48, 64)

foreach ($size in @(16, 20, 24, 32, 48, 64, 128, 256)) {
    & $render -SvgPath $svg -OutPath (Join-Path $iconDir "$size`x$size.png") -Size $size
}
Copy-Item (Join-Path $iconDir '256x256.png') (Join-Path $iconDir 'icon.png') -Force
Copy-Item (Join-Path $iconDir '256x256.png') (Join-Path $iconDir '128x128@2x.png') -Force
Remove-Item (Join-Path $iconDir '256x256.png') -Force

Push-Location $root
try {
    python (Join-Path $PSScriptRoot 'pack.py')
    python (Join-Path $PSScriptRoot 'make-status-icons.py') `
        --cloud (Join-Path $iconDir 'icon.png') `
        --out-dir $iconDir
    foreach ($size in $intermediates) {
        Remove-Item (Join-Path $iconDir "$size`x$size.png") -Force
    }
} finally {
    Pop-Location
}
Write-Host 'Pandock icon set regenerated.'

