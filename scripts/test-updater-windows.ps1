[CmdletBinding()]
param(
    [switch]$SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$temp = Join-Path $env:TEMP ("pandock-updater-e2e-" + [guid]::NewGuid().ToString("N"))
$fakeSource = Join-Path $root "updater\tests\fixtures\readiness_app.rs"
$noReadinessSource = Join-Path $root "updater\tests\fixtures\no_readiness_app.rs"
$fakeExe = Join-Path $temp "fake-app\pandock.exe"
$noReadinessExe = Join-Path $temp "fake-app\no-readiness.exe"
$helper = Join-Path $root "target\debug\pandock-updater.exe"

function Write-Text([string]$Path, [string]$Value) {
    $parent = Split-Path -Parent $Path
    if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    [System.IO.File]::WriteAllText($Path, $Value, [System.Text.UTF8Encoding]::new($false))
}

function New-Journal(
    [string]$AppDir,
    [string]$StagingDir,
    [string]$RollbackDir,
    [string]$HelperPath,
    [string]$ExpectedVersion,
    [uint32]$ParentPid
) {
    $now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
    $journal = [ordered]@{
        schema = 1
        protocol = 1
        sourceKind = "private_github"
        fromVersion = "1.0.0"
        toVersion = $ExpectedVersion
        appDir = $AppDir
        stagingDir = $StagingDir
        rollbackDir = $RollbackDir
        helperPath = $HelperPath
        parentPid = $ParentPid
        readinessToken = "e2e-readiness-token"
        state = "prepared"
        createdAt = $now
        updatedAt = $now
        error = $null
    }
    return ($journal | ConvertTo-Json -Depth 4)
}

function New-AppFixture([string]$AppDir) {
    New-Item -ItemType Directory -Path (Join-Path $AppDir "data") -Force | Out-Null
    Copy-Item -LiteralPath $fakeExe -Destination (Join-Path $AppDir "pandock.exe") -Force
    Write-Text (Join-Path $AppDir "WebView2Loader.dll") "old-dll"
    Write-Text (Join-Path $AppDir "VERSION.txt") "1.0.0`r`n"
    Write-Text (Join-Path $AppDir "LICENSE.txt") "license"
    Write-Text (Join-Path $AppDir "data\config.json") '{"keep":true}'
}

function New-StagingFixture([string]$StagingDir, [string]$HelperPath) {
    New-Item -ItemType Directory -Path $StagingDir -Force | Out-Null
    Copy-Item -LiteralPath $fakeExe -Destination (Join-Path $StagingDir "pandock.exe") -Force
    Write-Text (Join-Path $StagingDir "WebView2Loader.dll") "new-dll"
    Write-Text (Join-Path $StagingDir "VERSION.txt") "1.0.1`r`n"
    Write-Text (Join-Path $StagingDir "LICENSE.txt") "license"
    Copy-Item -LiteralPath $HelperPath -Destination (Join-Path $StagingDir "pandock-updater.exe") -Force
}

function Read-Journal([string]$Path) {
    return ([System.IO.File]::ReadAllText($Path, [System.Text.UTF8Encoding]::new($false)) | ConvertFrom-Json)
}

function New-Case([string]$Name) {
    $app = Join-Path $temp $Name
    $staging = Join-Path $app "updates\staging\1.0.1"
    $rollback = Join-Path $app "updates\rollback\1.0.0"
    New-AppFixture $app
    New-StagingFixture $staging $helper
    return @{
        App = $app
        Staging = $staging
        Rollback = $rollback
        Journal = (Join-Path $app "updates\update-journal.json")
    }
}

function Invoke-Helper($Case, [string]$Launch) {
    Write-Text $Case.Journal (New-Journal $Case.App $Case.Staging $Case.Rollback (Join-Path $Case.Staging "pandock-updater.exe") "1.0.1" 4294967294)
    & (Join-Path $Case.Staging "pandock-updater.exe") --protocol 1 --journal $Case.Journal --target-app $Case.App --launch $Launch --expected-version 1.0.1 --parent-pid 4294967294 --readiness-token e2e-readiness-token
    return $LASTEXITCODE
}

function Assert-RolledBack($Case) {
    if ((Get-Content -Raw (Join-Path $Case.App "VERSION.txt")).Trim() -ne "1.0.0") { throw "rollback did not restore version" }
    if ((Get-Content -Raw (Join-Path $Case.App "data\config.json")) -notmatch 'keep') { throw "rollback modified data" }
    $journal = Read-Journal $Case.Journal
    if ($journal.state -ne "rolled_back") { throw "expected rolled_back journal, got $($journal.state)" }
    if (-not (Test-Path -LiteralPath (Join-Path $Case.App "updates\readiness\e2e-readiness-token.json"))) { throw "old version was not restarted" }
}

try {
    if (-not $SkipBuild) {
        & cargo build -p pandock-updater
        if ($LASTEXITCODE -ne 0) { throw "updater build failed" }
    }
    New-Item -ItemType Directory -Path (Split-Path -Parent $fakeExe) -Force | Out-Null
    if (-not (Test-Path -LiteralPath $fakeExe)) {
        & rustc $fakeSource -o $fakeExe
        if ($LASTEXITCODE -ne 0) { throw "fake app build failed" }
    }
    if (-not (Test-Path -LiteralPath $noReadinessExe)) {
        & rustc $noReadinessSource -o $noReadinessExe
        if ($LASTEXITCODE -ne 0) { throw "no-readiness app build failed" }
    }
    if (-not (Test-Path -LiteralPath $helper)) { throw "helper not found: $helper" }

    # Success path.
    $case = New-Case "success-app"
    $code = Invoke-Helper $case (Join-Path $case.App "pandock.exe")
    if ($code -ne 0) { throw "success path helper failed" }
    if ((Get-Content -Raw (Join-Path $case.App "VERSION.txt")).Trim() -ne "1.0.1") { throw "version was not replaced" }
    if ((Read-Journal $case.Journal).state -ne "completed") { throw "success journal did not complete" }

    # New version spawn failure: corrupt new exe, old version must be restored and restarted.
    $case = New-Case "spawn-failure-app"
    [System.IO.File]::WriteAllText((Join-Path $case.Staging "pandock.exe"), "not-an-executable")
    $code = Invoke-Helper $case (Join-Path $case.App "pandock.exe")
    if ($code -eq 0) { throw "spawn failure path unexpectedly succeeded" }
    Assert-RolledBack $case

    # Readiness timeout: new process starts but never writes readiness; old version must restart.
    $case = New-Case "readiness-timeout-app"
    Copy-Item -LiteralPath $noReadinessExe -Destination (Join-Path $case.Staging "pandock.exe") -Force
    $previousTimeout = $env:PANDOCK_UPDATER_READINESS_TIMEOUT_MS
    try {
        $env:PANDOCK_UPDATER_READINESS_TIMEOUT_MS = "300"
        $code = Invoke-Helper $case (Join-Path $case.App "pandock.exe")
    }
    finally {
        if ($null -eq $previousTimeout) { Remove-Item Env:PANDOCK_UPDATER_READINESS_TIMEOUT_MS -ErrorAction SilentlyContinue }
        else { $env:PANDOCK_UPDATER_READINESS_TIMEOUT_MS = $previousTimeout }
    }
    if ($code -eq 0) { throw "readiness timeout path unexpectedly succeeded" }
    Assert-RolledBack $case

    # Old version exits immediately after rollback: must not be reported as success.
    $case = New-Case "old-version-exit-app"
    Copy-Item -LiteralPath $noReadinessExe -Destination (Join-Path $case.App "pandock.exe") -Force
    [System.IO.File]::WriteAllText((Join-Path $case.Staging "pandock.exe"), "not-an-executable")
    $code = Invoke-Helper $case (Join-Path $case.App "pandock.exe")
    if ($code -eq 0) { throw "old version immediate-exit path unexpectedly succeeded" }
    if ((Get-Content -Raw (Join-Path $case.App "VERSION.txt")).Trim() -ne "1.0.0") { throw "old files were not restored" }
    $journal = Read-Journal $case.Journal
    if ($journal.state -ne "failed" -or $journal.error -notmatch "old version exited immediately") { throw "old version immediate exit was not recorded as manual repair" }

    # Old launch failure: restore must happen, then explicit manual repair journal state.
    $case = New-Case "old-launch-failure-app"
    $code = Invoke-Helper $case (Join-Path $temp "missing.exe")
    if ($code -eq 0) { throw "old launch failure path unexpectedly succeeded" }
    if ((Get-Content -Raw (Join-Path $case.App "VERSION.txt")).Trim() -ne "1.0.0") { throw "old files were not restored" }
    $journal = Read-Journal $case.Journal
    if ($journal.state -ne "failed" -or $journal.error -notmatch "manual repair") { throw "old launch failure was not marked manual repair" }

    Write-Host "Windows updater e2e passed: success, spawn-failure rollback/relaunch, readiness-timeout rollback/relaunch, old-version-immediate-exit, manual-repair failure."
    exit 0
}
finally {
    if (Test-Path -LiteralPath $temp) { Remove-Item -LiteralPath $temp -Recurse -Force }
}
