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
