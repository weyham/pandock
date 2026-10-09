[CmdletBinding()]
param(
    [string]$WslDistro = $(if ($env:PANDOCK_WSL_DISTRO) { $env:PANDOCK_WSL_DISTRO } else { "Ubuntu-24.04" })
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$summaryPath = Join-Path $root "target/coverage-summary.json"

function ConvertTo-WslPath([string]$Path) {
    $fullPath = (Resolve-Path -LiteralPath $Path).Path
    if ($fullPath -notmatch '^([A-Za-z]):\\(.*)$') {
        throw "WSL coverage requires a local drive path, got: $fullPath"
    }
    $drive = $Matches[1].ToLowerInvariant()
    $tail = $Matches[2].Replace('\', '/')
    return "/mnt/$drive/$tail"
}

function Assert-FileCoverage($Report, [string]$Pattern, [double]$Minimum, [string]$Label) {
    $file = $Report.data[0].files | Where-Object {
        $_.filename -match $Pattern
    } | Select-Object -First 1
    if (-not $file) {
        throw "Coverage report did not contain $Label."
    }
    $percent = [double]$file.summary.lines.percent
    if ($percent -lt $Minimum) {
        throw "$Label line coverage is $([Math]::Round($percent, 2))%; expected >=$Minimum%."
    }
    return $percent
}

function Assert-Coverage([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "Coverage report was not generated: $Path"
    }
    $report = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    if (-not $report.data -or $report.data.Count -lt 1) {
        throw "Coverage report does not contain any data."
    }

    $totalLinePercent = [double]$report.data[0].totals.lines.percent
    $authFile = $report.data[0].files | Where-Object {
        $_.filename -match '[\\/]auth_flow\.rs$'
    } | Select-Object -First 1
    if (-not $authFile) {
        throw "Coverage report did not contain auth_flow.rs."
    }
    if ($totalLinePercent -lt 80) {
        throw "Total core line coverage is $([Math]::Round($totalLinePercent, 2))%; expected >=80%."
    }
    $authLinePercent = [double]$authFile.summary.lines.percent
    if ($authLinePercent -lt 90) {
        throw "auth_flow.rs line coverage is $([Math]::Round($authLinePercent, 2))%; expected >=90%."
    }

    $thresholds = @(
        @{ Pattern = '[\\/]update[\\/]cdn\.rs$'; Minimum = 90; Label = 'update/cdn.rs' },
        @{ Pattern = '[\\/]update[\\/]verify\.rs$'; Minimum = 90; Label = 'update/verify.rs' },
        @{ Pattern = '[\\/]update[\\/]journal\.rs$'; Minimum = 90; Label = 'update/journal.rs' },
        @{ Pattern = '[\\/]update[\\/]velopack_feed\.rs$'; Minimum = 90; Label = 'update/velopack_feed.rs' }
    )
    $checked = @()
    foreach ($threshold in $thresholds) {
        $value = Assert-FileCoverage $report $threshold.Pattern $threshold.Minimum $threshold.Label
        $checked += ("{0} {1:N2}%" -f $threshold.Label, $value)
    }

    Write-Host ("Coverage passed: core lines {0:N2}% (>=80%), auth_flow.rs lines {1:N2}% (>=90%), {2}." -f $totalLinePercent, $authLinePercent, ($checked -join ', '))
}

New-Item -ItemType Directory -Path (Split-Path -Parent $summaryPath) -Force | Out-Null

$isWindows = [Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT
if ($isWindows -and (Get-Command wsl.exe -ErrorAction SilentlyContinue) -and (wsl.exe -l -q 2>$null | Where-Object { $_.Trim() -ne "" })) {
    $linuxRoot = ConvertTo-WslPath $root
    $linuxSummary = ConvertTo-WslPath (Split-Path -Parent $summaryPath)
    $linuxSummary = "$linuxSummary/coverage-summary.json"
    $bash = @"
set -e
if [ -f "`$HOME/.cargo/env" ]; then . "`$HOME/.cargo/env"; fi
cd '$linuxRoot'
export CARGO_TARGET_DIR=/tmp/pandock-coverage-target
cargo llvm-cov -p pandock-core --json --summary-only --output-path '$linuxSummary' -- --test-threads=1
"@
    $bash = $bash.Replace("`r`n", "`n")
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = & wsl.exe -d $WslDistro -- bash -lc $bash 2>&1
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
    $output | ForEach-Object {
        if ($_ -is [System.Management.Automation.ErrorRecord]) {
            Write-Host $_.Exception.Message
        }
        else {
            Write-Host $_
        }
    }
    if ($exitCode -ne 0) {
        throw "cargo llvm-cov failed with exit code $exitCode."
    }
}
else {
    if (-not (Get-Command cargo-llvm-cov -ErrorAction SilentlyContinue)) {
        throw "cargo-llvm-cov is required for coverage checks."
    }
    Push-Location $root
    try {
        & cargo llvm-cov -p pandock-core --json --summary-only --output-path $summaryPath -- --test-threads=1
        if ($LASTEXITCODE -ne 0) {
            throw "cargo llvm-cov failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
    }
}

Assert-Coverage $summaryPath
