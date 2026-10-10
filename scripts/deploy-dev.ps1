# deploy-dev.ps1 — 把本地构建产物原子部署到 app\ 运行目录（开发迭代用）
#
# 纪律（AGENTS.md）：
# - app\ 实例的更新只允许走本脚本；禁止手工 kill+copy。
# - data\ 永不被部署触碰。
# - 只停 ExecutablePath 恰好等于 app\pandock.exe 的进程，绝不误杀其他实例。
#
# 用法：powershell -ExecutionPolicy Bypass -File .\scripts\deploy-dev.ps1 -SourceDir .\target\release-package\release
# （或先跑 build-velopack.ps1 后用 .\dist\velopack-stage——但该目录打包后即清理，
#  推荐直接用 release 构建输出目录）

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$SourceDir,
    [string]$RuntimeDir = ""
)

if ([string]::IsNullOrWhiteSpace($RuntimeDir)) {
    $RuntimeDir = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) "..\app"
}

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$src = (Resolve-Path -LiteralPath $SourceDir).Path
$dst = (Resolve-Path -LiteralPath $RuntimeDir).Path

# 只替换白名单文件；data\、updates\、日志等运行状态一概不动
$whitelist = @("pandock.exe", "updater.exe", "WebView2Loader.dll", "VERSION.txt", "LICENSE.txt", "README-portable.txt")
$present = @($whitelist | Where-Object { Test-Path -LiteralPath (Join-Path $src $_) })
if ($present -notcontains "pandock.exe") { throw "源目录缺少 pandock.exe：$src" }

$expectedHash = (Get-FileHash -LiteralPath (Join-Path $src "pandock.exe") -Algorithm SHA256).Hash

# 1. 备份当前运行文件（保留最近 2 份）
$backup = Join-Path $dst (".deploy-backup-" + (Get-Date -Format "yyyyMMdd-HHmmss"))
New-Item -ItemType Directory -Path $backup -Force | Out-Null
foreach ($f in $whitelist) {
    $cur = Join-Path $dst $f
    if (Test-Path -LiteralPath $cur) { Copy-Item -LiteralPath $cur -Destination $backup }
}
Get-ChildItem $dst -Directory -Filter ".deploy-backup-*" |
    Sort-Object Name -Descending | Select-Object -Skip 2 |
    ForEach-Object { Remove-Item -LiteralPath $_.FullName -Recurse -Force }

# 2. 只停路径精确等于 app\pandock.exe 的进程
$exePath = Join-Path $dst "pandock.exe"
$targets = Get-CimInstance Win32_Process -Filter "Name='pandock.exe'" |
    Where-Object { $_.ExecutablePath -eq $exePath }
foreach ($p in $targets) { Stop-Process -Id $p.ProcessId -Force }
if ($targets) { Start-Sleep -Seconds 2 }

# 3. 白名单替换 + 哈希复核
try {
    foreach ($f in $present) {
        Copy-Item -LiteralPath (Join-Path $src $f) -Destination (Join-Path $dst $f) -Force
    }
    $actualHash = (Get-FileHash -LiteralPath $exePath -Algorithm SHA256).Hash
    if ($actualHash -ne $expectedHash) { throw "部署后哈希不一致：$actualHash != $expectedHash" }
}
catch {
    Write-Host "部署失败，正在回滚：$_. 从 $backup 恢复"
    foreach ($f in $whitelist) {
        $b = Join-Path $backup $f
        if (Test-Path -LiteralPath $b) { Copy-Item -LiteralPath $b -Destination (Join-Path $dst $f) -Force }
    }
    throw
}

# 4. 重启并确认存活
Start-Process -FilePath $exePath -WorkingDirectory $dst -WindowStyle Hidden
Start-Sleep -Seconds 3
$alive = Get-CimInstance Win32_Process -Filter "Name='pandock.exe'" |
    Where-Object { $_.ExecutablePath -eq $exePath }
if (-not $alive) { throw "重启后未观察到 $exePath 进程" }
Write-Host "部署完成：$($present -join ', ') -> $dst（备份：$backup）"
