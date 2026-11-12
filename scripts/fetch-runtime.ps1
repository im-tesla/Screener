$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$vendorDir = Join-Path $projectRoot 'vendor'
New-Item -ItemType Directory -Force $vendorDir | Out-Null

function Assert-Hash([string]$Path, [string]$Expected) {
    if ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLower() -ne $Expected.ToLower()) {
        throw "SHA-256 mismatch: $Path"
    }
}

$ffmpegPath = Join-Path $vendorDir 'ffmpeg.exe'
$ffmpegHash = '57c56e369d5b4873b4d93fc1a1d833cb7cd8bc9325c14b05c34ce60b22842d8a'
if (!(Test-Path -LiteralPath $ffmpegPath)) {
    $installed = Get-Command ffmpeg.exe -ErrorAction SilentlyContinue
    if ($installed) {
        $candidate = Get-Item -LiteralPath $installed.Source
        $sourcePath = if ($candidate.LinkTarget) { $candidate.LinkTarget } else { $candidate.FullName }
        if ((Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLower() -eq $ffmpegHash) {
            Copy-Item -LiteralPath $sourcePath -Destination $ffmpegPath
        }
    }
    if (!(Test-Path -LiteralPath $ffmpegPath)) {
        $archive = Join-Path $vendorDir 'ffmpeg-9.0.1-full_build.7z'
        Invoke-WebRequest 'https://github.com/GyanD/codexffmpeg/releases/download/9.0.1/ffmpeg-9.0.1-full_build.7z' -OutFile $archive
        Assert-Hash $archive '4b9c814cb07a1f90d05b768ef4eb2abbf89af94bbb924df5b7dbd6e64e1e2b96'
        tar -xf $archive -C $vendorDir
        if ($LASTEXITCODE -ne 0) { throw 'FFmpeg archive extraction failed' }
        Copy-Item -LiteralPath (Join-Path $vendorDir 'ffmpeg-9.0.1-full_build\bin\ffmpeg.exe') -Destination $ffmpegPath
    }
}
Assert-Hash $ffmpegPath $ffmpegHash

$mpvPath = Join-Path $vendorDir 'libmpv-2.dll'
if (!(Test-Path -LiteralPath $mpvPath)) {
    $archive = Join-Path $vendorDir 'mpv-dev.7z'
    Invoke-WebRequest 'https://github.com/shinchiro/mpv-winbuild-cmake/releases/download/20261111/mpv-dev-x86_64-20261111-git-bcde6b93cc.7z' -OutFile $archive
    Assert-Hash $archive '273c079b11ea5575822f972756f4c9a71149a5c10130353abeb8586738938452'
    tar -xf $archive -C $vendorDir
    if ($LASTEXITCODE -ne 0) { throw 'libmpv archive extraction failed' }
}
Assert-Hash $mpvPath '88504ee5560942a955405e99dfb5971368e4cf851321f36565bf2a667271db1b'
Write-Host 'Pinned video runtimes verified.'
