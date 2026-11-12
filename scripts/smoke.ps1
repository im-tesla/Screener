# End-to-end hardware video and server restart test using the packaged binaries.
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$distDir = Join-Path $projectRoot 'dist'
$viewerLog = Join-Path $distDir 'viewer.log'
$offset = if (Test-Path -LiteralPath $viewerLog) { (Get-Item -LiteralPath $viewerLog).Length } else { 0 }
$serverOne = $null; $serverTwo = $null; $viewer = $null
try {
    $serverOne = Start-Process -FilePath (Join-Path $distDir 'server.exe') `
        -ArgumentList '--smoke-seconds','10' -WindowStyle Hidden -PassThru
    $viewer = Start-Process -FilePath (Join-Path $distDir 'viewer.exe') `
        -ArgumentList '127.0.0.1','--view-only','--modal-probe','--smoke-seconds','28' -WindowStyle Hidden -PassThru
    if (!$serverOne.WaitForExit(16000)) { throw 'First server did not exit on schedule' }
    if ($serverOne.ExitCode -ne 0) { throw "First server failed: $($serverOne.ExitCode)" }
    $leftover = Get-CimInstance Win32_Process -Filter "Name='ffmpeg.exe'" |
        Where-Object ParentProcessId -eq $serverOne.Id
    if ($leftover) { throw 'Encoder child survived server exit' }
    $serverTwo = Start-Process -FilePath (Join-Path $distDir 'server.exe') `
        -ArgumentList '--smoke-seconds','22' -WindowStyle Hidden -PassThru
    if (!$viewer.WaitForExit(32000)) { throw 'Viewer did not exit on schedule' }
    if ($viewer.ExitCode -ne 0) { throw "Viewer hardware smoke test failed: $($viewer.ExitCode)" }
    $bytes = [IO.File]::ReadAllBytes($viewerLog)
    $fresh = [Text.Encoding]::UTF8.GetString($bytes, [int]$offset, $bytes.Length - [int]$offset)
    $connections = ([regex]::Matches($fresh, 'Hardware decoder active: d3d11va')).Count
    if ($connections -lt 2) { throw 'Hardware playback did not resume after server restart' }
    if ($fresh -notmatch 'SMOKE: connected=true, hardware_decode=true, frame_seen=true') {
        throw 'Final hardware playback confirmation missing'
    }
    if ($fresh -notmatch 'MODAL PROBE: decoded=\d+ presented=\d+ passed=true') {
        throw 'Video stopped presenting inside nested window message loop'
    }
    Write-Host "PASS: $connections hardware-decoded sessions; reconnection; nested message-loop presentation; encoder job cleanup."
} finally {
    foreach ($process in @($viewer, $serverOne, $serverTwo)) {
        if ($process -and !$process.HasExited) { $process.Kill(); $process.WaitForExit() }
    }
}
