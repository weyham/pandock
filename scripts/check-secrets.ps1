[CmdletBinding()]
param(
    [string]$Root
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($Root)) {
    $Root = Split-Path -Parent $PSScriptRoot
}
if ([string]::IsNullOrWhiteSpace($Root)) {
    throw "Unable to resolve repository root."
}

$rootPath = (Resolve-Path -LiteralPath $Root).Path
$excludedSegments = @(".git", "data", "dist", "target", "node_modules", "gen")
$textExtensions = @(
    ".rs", ".toml", ".json", ".md", ".ps1", ".sh", ".ts", ".tsx",
    ".js", ".jsx", ".css", ".html", ".yml", ".yaml", ".txt", ".env"
)
$forbiddenExtensions = @(
    ".pem", ".key", ".p12", ".pfx", ".jks", ".keystore",
    ".mobileprovision", ".cer", ".crt"
)
$rules = @(
    @{ Name = "private-key"; Pattern = '-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----' },
    @{ Name = "github-token"; Pattern = '\bgh[pousr]_[A-Za-z0-9]{36,}\b' },
    @{ Name = "aws-access-key"; Pattern = '\bAKIA[0-9A-Z]{16}\b' },
    @{ Name = "google-api-key"; Pattern = '\bAIza[0-9A-Za-z\-_]{35}\b' },
    @{ Name = "bearer-token"; Pattern = '(?i)\bBearer\s+[A-Za-z0-9\-._~+/]{32,}={0,2}' },
    @{ Name = "assigned-secret"; Pattern = '(?i)\b(?:api[_-]?key|client[_-]?secret|app[_-]?secret|access[_-]?token|refresh[_-]?token|webdav[_-]?password)\s*[:=]\s*["''][A-Za-z0-9._~+/=-]{24,}["'']' }
)

function Test-ExcludedPath([string]$RelativePath) {
    $segments = $RelativePath -split '[\\/]'
    foreach ($segment in $segments) {
        if ($excludedSegments -contains $segment) {
            return $true
        }
    }
    return $false
}

$findings = [System.Collections.Generic.List[string]]::new()

foreach ($file in Get-ChildItem -LiteralPath $rootPath -Recurse -File -Force -ErrorAction SilentlyContinue) {
    $relative = $file.FullName.Substring($rootPath.Length).TrimStart([char[]]@('\','/'))
    if (Test-ExcludedPath $relative) {
        continue
    }

    $leaf = $file.Name.ToLowerInvariant()
    $extension = $file.Extension.ToLowerInvariant()

    if (($leaf -eq ".env" -or $leaf -like ".env.*") -and $leaf -ne ".env.example") {
        $findings.Add("${relative}:1 [forbidden-env-file]")
        continue
    }
    if ($forbiddenExtensions -contains $extension) {
        $findings.Add("${relative}:1 [forbidden-credential-file]")
        continue
    }
    if ($leaf -eq ".npmrc" -or $leaf -eq "credentials" -or $leaf -eq "credentials.toml") {
        $findings.Add("${relative}:1 [forbidden-credential-file]")
        continue
    }
    if (-not ($textExtensions -contains $extension -or $extension -eq "")) {
        continue
    }
    if ($file.Length -gt 2MB) {
        continue
    }

    try {
        $lines = [System.IO.File]::ReadAllLines($file.FullName)
    }
    catch {
        continue
    }

    for ($lineIndex = 0; $lineIndex -lt $lines.Length; $lineIndex++) {
        foreach ($rule in $rules) {
            if ([regex]::IsMatch($lines[$lineIndex], $rule.Pattern)) {
                $findings.Add("${relative}:$($lineIndex + 1) [$($rule.Name)]")
            }
        }
    }
}

if ($findings.Count -gt 0) {
    $findings | Sort-Object -Unique | ForEach-Object { Write-Host $_ }
    throw "Secret scan failed with $($findings.Count) finding(s)."
}

Write-Host "Secret scan passed."
