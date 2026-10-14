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

    # NativeSync 引擎单测（Windows Cloud Files 回调/路径/索引/状态机）。
    # 不设覆盖率阈值：其主体在 cfg(windows) 下，跨平台统计会得到假指标（ADR-109）。
    & cargo test -p pandock-nativesync
    if ($LASTEXITCODE -ne 0) { throw "nativesync tests failed with exit code $LASTEXITCODE" }

    # app lib 测试（ADR-113 定稿）：显式 rustup MSVC 运行——本机 PATH 是独立 GNU，
    # GNU 下该测试二进制 0xC0000139 加载失败（docs/validation.md「门禁覆盖面差异」
    # 有历史记录）；MSVC 下实证 8 passed。MSVC 与 GNU 产物在共享 target\ 下按
    # 指纹共存（首次 MSVC 构建会多花几分钟，属预期）。
    $msvcToolchain = "stable-x86_64-pc-windows-msvc"
    $rustup = Resolve-PandockRustup
    $toolchainList = (& $rustup toolchain list 2>$null) -join "`n"
    if ($toolchainList -notmatch [regex]::Escape($msvcToolchain)) {
        throw "缺少 Rust MSVC 工具链（$msvcToolchain）：app lib 测试要求它（ADR-113）。请先执行: rustup toolchain install $msvcToolchain"
    }
    & $rustup run $msvcToolchain cargo test -p pandock-app --lib
    if ($LASTEXITCODE -ne 0) { throw "app lib tests (MSVC) failed with exit code $LASTEXITCODE" }

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
