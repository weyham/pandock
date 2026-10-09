[CmdletBinding()]
param(
    [switch]$SkipUiInstall,
    [switch]$SkipCoverage,
    [switch]$WithLegacyUpdater
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot "common.ps1")
Initialize-PandockBuildEnvironment

Push-Location $root
try {
    if (-not $SkipUiInstall) {
        & npm --prefix ui ci
        if ($LASTEXITCODE -ne 0) { throw "npm ci failed with exit code $LASTEXITCODE" }
    }
    elseif (-not (Test-Path -LiteralPath "ui\node_modules")) {
        throw "ui/node_modules is missing; run without -SkipUiInstall first."
    }

    & (Join-Path $PSScriptRoot "check-version.ps1")

    & cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt failed with exit code $LASTEXITCODE" }

    & cargo clippy --workspace --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed with exit code $LASTEXITCODE" }

    & cargo test -p pandock-core
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed with exit code $LASTEXITCODE" }

    & cargo check -p pandock-app
    if ($LASTEXITCODE -ne 0) { throw "cargo check failed with exit code $LASTEXITCODE" }

    & cargo check -p pandock-updater
    if ($LASTEXITCODE -ne 0) { throw "cargo check updater failed with exit code $LASTEXITCODE" }

    & (Join-Path $PSScriptRoot "test-windows-detection.ps1")
    if ($LASTEXITCODE -ne 0) { throw "Windows detection self-test failed" }
    $isWindows = Test-PandockWindows
    # portable 自更新的 journal/helper 交换 e2e（ADR-102/ADR-106），按需显式运行。
    if ($isWindows -and $WithLegacyUpdater) {
        & cargo build -p pandock-updater
        if ($LASTEXITCODE -ne 0) { throw "cargo build updater failed with exit code $LASTEXITCODE" }
        & (Join-Path $PSScriptRoot "test-updater-windows.ps1") -SkipBuild
        if ($LASTEXITCODE -ne 0) { throw "Windows updater e2e failed with exit code $LASTEXITCODE" }
        Write-Host "Windows updater e2e passed"
    }

    if (-not $SkipCoverage) {
        & (Join-Path $PSScriptRoot "check-coverage.ps1")
    }

    & npm --prefix ui test
    if ($LASTEXITCODE -ne 0) { throw "UI tests failed with exit code $LASTEXITCODE" }

    & npm --prefix ui run build
    if ($LASTEXITCODE -ne 0) { throw "UI build failed with exit code $LASTEXITCODE" }

    & (Join-Path $PSScriptRoot "check-secrets.ps1") -Root $root
    Write-Host "All local checks passed."
}
finally {
    Pop-Location
}
