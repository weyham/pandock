# deploy-dev.ps1 — 把本地构建产物原子部署到 app\ 运行目录（开发迭代用）
#
# 纪律（AGENTS.md）：
# - app\ 实例的更新只允许走本脚本；禁止手工 kill+copy。
# - data\ 永不被部署触碰。
# - 只停 ExecutablePath 恰好等于 <RuntimeDir>\pandock.exe 的进程，绝不误杀其他实例。
#
# 用法：powershell -ExecutionPolicy Bypass -File .\scripts\deploy-dev.ps1 -SourceDir .\target\release-package\release
# （或先跑 build-velopack.ps1 后用 .\dist\velopack-stage——但该目录打包后即清理，
#  推荐直接用 release 构建输出目录）
#
# 2026-10-12 硬化（事故：目标 exe 被 Defender/残留句柄锁住时，回滚分支也撞同一把锁，
# 脚本在第 3 步抛错、没走到重启 ⇒ 用户实例被停约 27 秒）：
#   ① 拷贝前 pre-flight：目标文件必须能被**独占打开**，否则在**停进程之前**就退出（不打断服务）；
#   ② Stop-Process 后**轮询解锁**（上限 30 秒），不再固定 sleep 2；
#   ③ 拷贝失败：**先还原文件**，再**把服务拉起来**，最后才报错（绝不留下停着的实例）。
# 演练开关：-FaultInjectCopyFail 且环境变量 PANDOCK_DEPLOY_REHEARSAL=1 时，在拷贝阶段注入失败，
# 用于在**临时 RuntimeDir** 上演练"锁定→重试→恢复"三条路径。生产用不到，双重门槛避免误用。

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$SourceDir,
    [string]$RuntimeDir = "",
    [switch]$FaultInjectCopyFail
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# 独占打开探测：$true = 可独占（未被占用/可写）。文件不存在也视为可写。
function Test-FileExclusive([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $true }
    try {
        $fs = [System.IO.File]::Open(
            $Path,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::ReadWrite,
            [System.IO.FileShare]::None)
        $fs.Close()
        return $true
    }
    catch {
        return $false
    }
}

# 轮询直到全部目标文件可独占打开（替代固定 sleep）。
function Wait-FileExclusive([string[]]$Paths, [int]$TimeoutSeconds) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $locked = @($Paths | Where-Object { -not (Test-FileExclusive $_) })
        if ($locked.Count -eq 0) { return $true }
        Start-Sleep -Milliseconds 500
    }
    return $false
}

if ([string]::IsNullOrWhiteSpace($RuntimeDir)) {
    $RuntimeDir = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) "..\app"
}

$src = (Resolve-Path -LiteralPath $SourceDir).Path
$dst = (Resolve-Path -LiteralPath $RuntimeDir).Path

# 只替换白名单文件；data\、updates\、日志等运行状态一概不动
$whitelist = @("pandock.exe", "updater.exe", "WebView2Loader.dll", "VERSION.txt", "LICENSE.txt", "README-portable.txt")
$present = @($whitelist | Where-Object { Test-Path -LiteralPath (Join-Path $src $_) })
if ($present -notcontains "pandock.exe") { throw "源目录缺少 pandock.exe：$src" }

$expectedHash = (Get-FileHash -LiteralPath (Join-Path $src "pandock.exe") -Algorithm SHA256).Hash
$exePath = Join-Path $dst "pandock.exe"

# ---- ① pre-flight：目标被占用 ⇒ 在停进程之前就退出（不打断服务）----
# 注意：`pandock.exe` 此刻**正被"我们自己要停掉的实例"锁着**（进程映像只允许读共享），
# 因此**不**把它纳入这一步——它由"停进程后的轮询解锁"（②）把关：
#   * 运行中实例造成的锁：Stop-Process 之后会立刻释放（实测 0.5 秒）；
#   * 第三方（Defender/残留句柄）造成的锁：②轮询 30 秒后仍不放 ⇒ 恢复服务并报错。
# 这一步只探测"不会被我们停掉"的白名单文件（updater.exe / DLL / 文本）。
$existing = @($present | ForEach-Object { Join-Path $dst $_ } | Where-Object { Test-Path -LiteralPath $_ })
$preflight = @($present | Where-Object { $_ -ne "pandock.exe" } |
    ForEach-Object { Join-Path $dst $_ } | Where-Object { Test-Path -LiteralPath $_ })
$lockedNow = @($preflight | Where-Object { -not (Test-FileExclusive $_) })
if ($lockedNow.Count -gt 0) {
    throw "目标文件被占用（未停止任何进程）：$($lockedNow -join ', ')。请稍后重试。"
}

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

# 2. 只停路径精确等于 <RuntimeDir>\pandock.exe 的进程
$targets = Get-CimInstance Win32_Process -Filter "Name='pandock.exe'" |
    Where-Object { $_.ExecutablePath -eq $exePath }
foreach ($p in $targets) { Stop-Process -Id $p.ProcessId -Force }
if ($targets) {
    Write-Host "已停止实例：$($targets.ProcessId -join ', ')（$(Get-Date -Format 'HH:mm:ss')）"
    # ---- ② 轮询解锁（上限 30 s），不再固定 sleep 2 ----
    $unlocked = Wait-FileExclusive -Paths $existing -TimeoutSeconds 30
    if (-not $unlocked) {
        $still = @($existing | Where-Object { -not (Test-FileExclusive $_) })
        # ---- ③ 先恢复服务，再报错 ----
        if (Test-Path -LiteralPath $exePath) {
            Start-Process -FilePath $exePath -WorkingDirectory $dst -WindowStyle Hidden
            Write-Host "文件仍被占用（$($still -join ', ')），已重启实例以恢复服务"
        }
        throw "停止进程后目标文件在 30 秒内仍未解锁：$($still -join ', ')"
    }
    Write-Host "目标文件已解锁（$(Get-Date -Format 'HH:mm:ss')）"
}

# 3. 白名单替换 + 哈希复核；失败则还原文件并恢复服务
try {
    $copied = 0
    foreach ($f in $present) {
        Copy-Item -LiteralPath (Join-Path $src $f) -Destination (Join-Path $dst $f) -Force
        $copied++
        # 演练开关（双重门槛）：在**已拷贝至少一个文件之后**注入失败，好让"还原"路径可观测。
        if ($FaultInjectCopyFail -and $env:PANDOCK_DEPLOY_REHEARSAL -eq "1" -and $copied -ge 1) {
            throw "rehearsal: 注入的拷贝失败（-FaultInjectCopyFail，仅演练用）"
        }
    }
    $actualHash = (Get-FileHash -LiteralPath $exePath -Algorithm SHA256).Hash
    if ($actualHash -ne $expectedHash) { throw "部署后哈希不一致：$actualHash != $expectedHash" }
}
catch {
    $reason = $_
    Write-Host "部署失败，正在还原：$reason"
    foreach ($f in $whitelist) {
        $b = Join-Path $backup $f
        if (Test-Path -LiteralPath $b) { Copy-Item -LiteralPath $b -Destination (Join-Path $dst $f) -Force }
    }
    # ---- ③ 还原之后必须把服务拉起来，再报错 ----
    $restored = $false
    if (Test-Path -LiteralPath $exePath) {
        Start-Process -FilePath $exePath -WorkingDirectory $dst -WindowStyle Hidden
        Start-Sleep -Seconds 2
        $alive = Get-CimInstance Win32_Process -Filter "Name='pandock.exe'" |
            Where-Object { $_.ExecutablePath -eq $exePath }
        $restored = [bool]$alive
    }
    if ($restored) { Write-Host "已还原文件并重启实例（服务已恢复），备份：$backup" }
    else { Write-Host "已还原文件，但未能确认实例存活（备份：$backup）" }
    throw $reason
}

# 4. 重启并确认存活
Start-Process -FilePath $exePath -WorkingDirectory $dst -WindowStyle Hidden
Start-Sleep -Seconds 3
$alive = Get-CimInstance Win32_Process -Filter "Name='pandock.exe'" |
    Where-Object { $_.ExecutablePath -eq $exePath }
if (-not $alive) { throw "重启后未观察到 $exePath 进程" }
Write-Host "部署完成：$($present -join ', ') -> $dst（备份：$backup）"
