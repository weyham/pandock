[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$Upload,
    [switch]$Publish
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot "common.ps1")
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)

function Read-CargoVersion([string]$Path) {
    $text = [System.IO.File]::ReadAllText((Join-Path $root $Path), $utf8NoBom)
    $match = [regex]::Match($text, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $match.Success) { throw "Version not found in $Path" }
    return $match.Groups[1].Value
}

Push-Location $root
try {
    $version = Read-CargoVersion "Cargo.toml"
    $stageDir = Join-Path $root "target\velopack-stage"
    $outDir = Join-Path $root "dist\velopack"

    if (-not $SkipBuild) {
        $env:CARGO_TARGET_DIR = Join-Path $root "target\release-package"
        $env:RUSTFLAGS = "-C link-arg=-Wl,--no-insert-timestamp"

        & npm --prefix ui run build
        if ($LASTEXITCODE -ne 0) { throw "UI build failed with exit code $LASTEXITCODE" }

        & cargo build --release -p pandock-app --features custom-protocol
        if ($LASTEXITCODE -ne 0) { throw "Release build failed with exit code $LASTEXITCODE" }

        # portable 就地更新 helper（ADR-102），随 nupkg 与便携包分发
        & cargo build --release -p pandock-updater
        if ($LASTEXITCODE -ne 0) { throw "Updater build failed with exit code $LASTEXITCODE" }

        $exe = Join-Path $env:CARGO_TARGET_DIR "release\pandock.exe"
        if (-not (Test-Path -LiteralPath $exe)) { throw "Release executable not found: $exe" }
        & (Join-Path $PSScriptRoot "verify-windows-resource.ps1") -ExePath $exe
        if ($LASTEXITCODE -ne 0) { throw "Windows resource verification failed with exit code $LASTEXITCODE" }
    }

    $builtExe = Join-Path $root "target\release-package\release\pandock.exe"
    $builtDll = Join-Path $root "target\release-package\release\WebView2Loader.dll"
    $builtUpdater = Join-Path $root "target\release-package\release\pandock-updater.exe"
    foreach ($required in @($builtExe, $builtDll, $builtUpdater)) {
        if (-not (Test-Path -LiteralPath $required)) { throw "Missing built file: $required (run without -SkipBuild)" }
    }

    if (Test-Path -LiteralPath $stageDir) { Remove-Item -Recurse -Force $stageDir }
    New-Item -ItemType Directory -Path $stageDir -Force | Out-Null
    Copy-Item -LiteralPath $builtExe, $builtDll -Destination $stageDir
    Copy-Item -LiteralPath $builtUpdater -Destination (Join-Path $stageDir "updater.exe")

    if (Test-Path -LiteralPath $outDir) { Remove-Item -Recurse -Force $outDir }
    New-Item -ItemType Directory -Path $outDir -Force | Out-Null

    $splash = Join-Path $root "packaging\velopack\splash.png"
    if (-not (Test-Path -LiteralPath $splash)) { throw "Splash image not found: $splash (run packaging\velopack\make-splash.py)" }

    & vpk pack `
        --packId Pandock `
        --packVersion $version `
        --packDir $stageDir `
        --outputDir $outDir `
        --packTitle "Pandock" `
        --packAuthors "weyham" `
        --mainExe pandock.exe `
        --icon (Join-Path $root "src-tauri\icons\icon.ico") `
        --splashImage $splash `
        --shortcuts StartMenuRoot `
        -y
    if ($LASTEXITCODE -ne 0) { throw "vpk pack failed with exit code $LASTEXITCODE" }

    # 规范便携包：自带空 data\ 触发 portable 数据目录（ADR-106 规则 3），
    # 直接覆盖 vpk 默认产出的 Portable.zip（不含 data\，不是真正便携行为），
    # 文件名保持 vpk 约定，否则 vpk upload 找不到预期资产。
    $portableName = "Pandock-win-Portable"
    $portableStage = Join-Path $root "target\velopack-portable-stage"
    if (Test-Path -LiteralPath $portableStage) { Remove-Item -Recurse -Force $portableStage }
    New-Item -ItemType Directory -Path (Join-Path $portableStage "data") -Force | Out-Null
    Copy-Item -LiteralPath $builtExe, $builtDll -Destination $portableStage
    Copy-Item -LiteralPath $builtUpdater -Destination (Join-Path $portableStage "updater.exe")
    Copy-Item -LiteralPath (Join-Path $root "LICENSE") -Destination (Join-Path $portableStage "LICENSE.txt")
    [System.IO.File]::WriteAllText((Join-Path $portableStage "VERSION.txt"), "$version`r`n", $utf8NoBom)
    [System.IO.File]::WriteAllText((Join-Path $portableStage "data\README.txt"), "Pandock 运行时数据目录，请勿手动修改。`r`n更新便携版时保留本目录即可。`r`n", $utf8NoBom)
    $portableReadme = @"
Pandock $version Windows x64 便携版

1. 解压到可写目录。
2. 运行 pandock.exe。
3. 从系统托盘打开设置，填写百度开放平台 App Key / Secret Key / 应用名。
4. 完成设备码授权。
5. 挂载 http://127.0.0.1:19090/dav/。

请保持 pandock.exe、WebView2Loader.dll 和 data\ 在一起。
运行时数据保存在 exe 旁的 data\ 中；机密和令牌保存在当前 Windows 用户的凭据管理器中。
便携版不提供应用内自动更新，请前往 Releases 页面下载新版本，解压时保留 data\ 目录。
"@
    [System.IO.File]::WriteAllText((Join-Path $portableStage "README-portable.txt"), $portableReadme, $utf8NoBom)

    $portableZip = Join-Path $outDir "$portableName.zip"
    if (Test-Path -LiteralPath $portableZip) { Remove-Item -Force $portableZip }
    Compress-Archive -Path (Join-Path $portableStage "*") -DestinationPath $portableZip
    Remove-Item -Recurse -Force $portableStage

    $setup = Join-Path $outDir "Pandock-win-Setup.exe"
    $hash = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText("$setup.sha256", "$hash  Pandock-win-Setup.exe`r`n", $utf8NoBom)
    $portableHash = (Get-FileHash -LiteralPath $portableZip -Algorithm SHA256).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText("$portableZip.sha256", "$portableHash  $portableName.zip`r`n", $utf8NoBom)

    Write-Host "Setup:    $setup"
    Write-Host "SHA-256:  $hash"
    Write-Host "Portable: $portableZip"
    Write-Host "SHA-256:  $portableHash"

    if ($Upload) {
        $token = $env:GH_TOKEN
        if ([string]::IsNullOrWhiteSpace($token)) {
            $token = (& gh auth token) | Select-Object -First 1
        }
        if ([string]::IsNullOrWhiteSpace($token)) { throw "No GitHub token available for upload (gh auth token failed)." }
        $uploadArgs = @(
            "upload", "github",
            "--repoUrl", "https://github.com/weyham/pandock",
            "--token", $token,
            "--outputDir", $outDir,
            "-y"
        )
        if ($Publish) { $uploadArgs += "--publish" }
        & vpk @uploadArgs
        if ($LASTEXITCODE -ne 0) { throw "vpk upload failed with exit code $LASTEXITCODE" }
        $mode = if ($Publish) { "published" } else { "uploaded as draft" }
        Write-Host "Release ${version} $mode on weyham/pandock"
    }
}
finally {
    Pop-Location
}