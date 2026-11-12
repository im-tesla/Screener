$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $projectRoot
try {
    if (!(Test-Path vendor\ffmpeg.exe) -or !(Test-Path vendor\libmpv-2.dll)) {
        & (Join-Path $PSScriptRoot 'fetch-runtime.ps1')
    }
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Rust build failed' }
    New-Item -ItemType Directory -Force dist | Out-Null
    Copy-Item -LiteralPath target\release\server.exe,target\release\viewer.exe -Destination dist
    Copy-Item -LiteralPath README.md,LICENSE,THIRD_PARTY.md,VALIDATION.md -Destination dist
    Copy-Item -LiteralPath scripts\allow-lan.ps1 -Destination dist
    Get-FileHash dist\server.exe,dist\viewer.exe -Algorithm SHA256 |
        Select-Object @{Name='File';Expression={Split-Path -Leaf $_.Path}},Hash |
        ConvertTo-Json | Set-Content -Encoding utf8 dist\checksums.json
    Write-Host 'Ready: dist\server.exe and dist\viewer.exe. Each is self-contained.'
} finally { Pop-Location }
