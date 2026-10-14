[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ExePath
)

<#
.SYNOPSIS
    ADR-113 显形防线：断言给定 PE（pandock.exe）**不**依赖 WebView2Loader.dll。
    MSVC 静态链（WebView2LoaderStatic.lib）后，发布物不再随包携带该 DLL；
    若工具链被意外切回 GNU（或 GitHub 把默认主机工具链改了），导入表会重新出现
    WebView2Loader.dll —— 本脚本让这一回归在构建期直接显形失败。

    优先使用 VS Build Tools 的 dumpbin /dependents；dumpbin 不在时回退纯
    PowerShell 解析 PE 导入目录（IMAGE_DIRECTORY_ENTRY_IMPORT）。
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $ExePath)) {
    Write-Error "PE file not found: $ExePath"
    exit 2
}

$targetName = "WebView2Loader.dll"

function Test-WithDumpbin {
    $dumpbin = $null
    $dumpbinCmd = Get-Command dumpbin -ErrorAction SilentlyContinue
    if ($dumpbinCmd) { $dumpbin = $dumpbinCmd }
    if (-not $dumpbin) {
        # dumpbin 随 VS Build Tools 提供，不在 PATH 时尝试 vswhere 定位（vswhere 可能缺失）。
        $vswhereCmd = Get-Command vswhere.exe -ErrorAction SilentlyContinue
        if ($vswhereCmd) {
            $installPath = (& $vswhereCmd.Source -latest -products * -property installationPath 2>$null)
            if ($installPath) {
                $candidate = Get-ChildItem -LiteralPath (Join-Path $installPath "VC\Tools\MSVC") -Recurse -Filter dumpbin.exe -ErrorAction SilentlyContinue |
                    Where-Object { $_.FullName -match "x64" } | Select-Object -First 1
                if ($candidate) { $dumpbin = $candidate }
            }
        }
    }
    if (-not $dumpbin) { return $null }
    $output = & $dumpbin.Source /dependents $ExePath 2>$null
    if ($LASTEXITCODE -ne 0) { return $null }
    return (($output -join "`n") -match [regex]::Escape($targetName))
}

function Test-WithPEParsing {
    Add-Type -TypeDefinition @"
using System;
using System.IO;
public static class PeImportCheck {
    public static bool HasImport(string path, string dllName) {
        byte[] data = File.ReadAllBytes(path);
        if (data.Length < 0x40) return false;
        if (data[0] != 0x4D || data[1] != 0x5A) return false; // MZ
        int peOffset = BitConverter.ToInt32(data, 0x3C);
        if (peOffset <= 0 || peOffset + 0x18 >= data.Length) return false;
        if (data[peOffset] != 0x50 || data[peOffset + 1] != 0x45) return false; // PE signature
        int coff = peOffset + 4;
        int opt = coff + 20;
        ushort magic = BitConverter.ToUInt16(data, opt);
        int ddBase;
        if (magic == 0x10B) { ddBase = opt + 96; }
        else if (magic == 0x20B) { ddBase = opt + 112; }
        else { return false; }
        int importRva = BitConverter.ToInt32(data, ddBase + 8);
        if (importRva == 0) return false;
        // section table follows optional header
        ushort numSections = BitConverter.ToUInt16(data, coff + 2);
        ushort optSize = BitConverter.ToUInt16(data, coff + 16);
        int secTable = coff + 20 + optSize;
        Func<int, int> rvaToOffset = null;
        rvaToOffset = (rva) => {
            for (int i = 0; i < numSections; i++) {
                int sh = secTable + i * 40;
                uint vsize = BitConverter.ToUInt32(data, sh + 8);
                uint vaddr = BitConverter.ToUInt32(data, sh + 12);
                uint rawSize = BitConverter.ToUInt32(data, sh + 16);
                uint rawPtr = BitConverter.ToUInt32(data, sh + 20);
                if (rva >= vaddr && rva < vaddr + Math.Max(vsize, rawSize)) {
                    return (int)(rawPtr + (rva - vaddr));
                }
            }
            return -1;
        };
        int descOffset = rvaToOffset(importRva);
        if (descOffset < 0) return false;
        string dllLower = dllName.ToLowerInvariant();
        for (int i = 0; i < 256; i++) {
            int desc = descOffset + i * 20;
            if (desc + 20 > data.Length) break;
            uint nameRva = BitConverter.ToUInt32(data, desc + 12);
            if (nameRva == 0) break; // NULL descriptor
            int nameOff = rvaToOffset((int)nameRva);
            if (nameOff < 0) continue;
            int end = nameOff;
            while (end < data.Length && data[end] != 0) end++;
            string name = System.Text.Encoding.ASCII.GetString(data, nameOff, end - nameOff);
            if (name.Equals(dllName, StringComparison.OrdinalIgnoreCase)) return true;
        }
        return false;
    }
}
"@
    return [PeImportCheck]::HasImport($ExePath, $targetName)
}

$hasDependency = $null
$method = "dumpbin"
try {
    $hasDependency = Test-WithDumpbin
} catch {
    $hasDependency = $null
}
if ($null -eq $hasDependency) {
    $hasDependency = Test-WithPEParsing
    $method = "pe-parsing"
}

if ($hasDependency) {
    Write-Error "FAIL: $ExePath still depends on $targetName (expected MSVC static link) [method=$method]"
    exit 1
}
Write-Host "OK: $ExePath does not depend on $targetName [method=$method]"
exit 0
