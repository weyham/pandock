param(
    [Parameter(Mandatory=$true)][string]$SvgPath,
    [Parameter(Mandatory=$true)][string]$OutPath,
    [Parameter(Mandatory=$true)][int]$Size
)
$ErrorActionPreference = 'Stop'
$chrome = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
if (-not (Test-Path $chrome)) {
    $chrome = @(
        'C:\Program Files (x86)\Google\Chrome\Application\chrome.exe',
        "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe",
        'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe',
        'C:\Program Files\Microsoft\Edge\Application\msedge.exe'
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $chrome) { throw 'no Chrome/Edge binary found for SVG rendering' }

$svg = [System.IO.File]::ReadAllText($SvgPath)
$svg = $svg.Substring($svg.IndexOf('<svg'))
$html = @"
<!doctype html><html><head><meta charset="utf-8"><style>
html,body{margin:0;padding:0;background:transparent;overflow:hidden}
svg{display:block;width:${Size}px;height:${Size}px}
</style></head><body>$svg</body></html>
"@
$tmpHtml = Join-Path $env:TEMP ("render-" + [guid]::NewGuid().ToString("N") + ".html")
$tmpErr = Join-Path $env:TEMP ("render-" + [guid]::NewGuid().ToString("N") + ".err")
[System.IO.File]::WriteAllText($tmpHtml, $html, [System.Text.UTF8Encoding]::new($false))

$outDir = Split-Path -Parent $OutPath
if ($outDir -and -not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir -Force | Out-Null }
if (Test-Path $OutPath) { Remove-Item $OutPath -Force }

$uri = 'file:///' + ($tmpHtml -replace '\\','/')
$args = @(
    '--headless=new', '--disable-gpu', '--hide-scrollbars', '--force-device-scale-factor=1',
    "--window-size=$Size,$Size", '--default-background-color=00000000',
    "--screenshot=$OutPath", $uri
)
$proc = Start-Process -FilePath $chrome -ArgumentList $args -Wait -NoNewWindow -PassThru -RedirectStandardError $tmpErr
Remove-Item $tmpHtml -Force -ErrorAction SilentlyContinue
Remove-Item $tmpErr -Force -ErrorAction SilentlyContinue
if (-not (Test-Path $OutPath)) { throw "render failed for size $Size (chrome exit $($proc.ExitCode))" }
