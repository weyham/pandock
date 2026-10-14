function Initialize-PandockBuildEnvironment {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        return
    }
    if (Get-Command windres -ErrorAction SilentlyContinue) {
        return
    }

    $candidates = @()
    if (-not [string]::IsNullOrWhiteSpace($env:PANDOCK_MINGW_BIN)) {
        $candidates += $env:PANDOCK_MINGW_BIN
    }
    $candidates += @(
        "C:\mingw64\bin",
        (Join-Path $env:ProgramFiles "mingw64\bin")
    )

    foreach ($candidate in $candidates | Select-Object -Unique) {
        if (-not [string]::IsNullOrWhiteSpace($candidate) -and (Test-Path -LiteralPath (Join-Path $candidate "windres.exe"))) {
            $env:PATH = "$candidate;$env:PATH"
            return
        }
    }
}

function Test-PandockWindows {
    return [Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT
}

# ADR-113：发布/门禁脚本需要显式调 rustup（钉 MSVC 工具链），但本机 rustup 不一定
# 在 PATH（独立 GNU 的 bin 在、.cargo\bin 不在）。解析顺序：PATH → 标准安装位置 → 明确报错。
function Resolve-PandockRustup {
    $cmd = Get-Command rustup -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $fallback = Join-Path $env:USERPROFILE ".cargo\bin\rustup.exe"
    if (Test-Path -LiteralPath $fallback) { return $fallback }
    throw "找不到 rustup：PATH 与 USERPROFILE\.cargo\bin 均无。请先安装 rustup（https://rustup.rs/）并执行: rustup toolchain install stable-x86_64-pc-windows-msvc"
}
