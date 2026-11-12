# End-to-end hardware video, UDP loss recovery, desktop audio, and restart test.
param([string]$DistDir = 'dist')
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$distDir = if ([IO.Path]::IsPathRooted($DistDir)) { $DistDir } else { Join-Path $projectRoot $DistDir }
$viewerLog = Join-Path $distDir 'viewer.log'
$offset = if (Test-Path -LiteralPath $viewerLog) { (Get-Item -LiteralPath $viewerLog).Length } else { 0 }
$serverOne = $null; $serverTwo = $null; $serverThree = $null; $serverFour = $null
$viewer = $null; $lossViewer = $null; $audioViewer = $null
function Read-Fresh([int]$from) {
    # Share read/write/delete: another viewer may still be appending to this log.
    $stream = [IO.File]::Open($viewerLog, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
    try {
        $length = [int]$stream.Length
        $bytes = New-Object byte[] $length
        [void]$stream.Read($bytes, 0, $length)
    } finally {
        $stream.Dispose()
    }
    if ($length -le $from) { return '' }
    return [Text.Encoding]::UTF8.GetString($bytes, $from, $length - $from)
}
try {
    $serverOne = Start-Process -FilePath (Join-Path $distDir 'server.exe') `
        -ArgumentList '--smoke-seconds','10' -WindowStyle Hidden -PassThru
    $viewer = Start-Process -FilePath (Join-Path $distDir 'viewer.exe') `
        -ArgumentList '127.0.0.1','--view-only','--no-audio','--modal-probe','--smoke-seconds','28' -WindowStyle Hidden -PassThru
    if (!$serverOne.WaitForExit(16000)) { throw 'First server did not exit on schedule' }
    if ($serverOne.ExitCode -ne 0) { throw "First server failed: $($serverOne.ExitCode)" }
    $leftover = Get-CimInstance Win32_Process -Filter "Name='ffmpeg.exe'" |
        Where-Object ParentProcessId -eq $serverOne.Id
    if ($leftover) { throw 'Encoder child survived server exit' }
    $serverTwo = Start-Process -FilePath (Join-Path $distDir 'server.exe') `
        -ArgumentList '--smoke-seconds','22' -WindowStyle Hidden -PassThru
    if (!$viewer.WaitForExit(32000)) { throw 'Viewer did not exit on schedule' }
    if ($viewer.ExitCode -ne 0) { throw "Viewer hardware smoke test failed: $($viewer.ExitCode)" }
    $fresh = Read-Fresh ([int]$offset)
    $connections = ([regex]::Matches($fresh, 'Hardware decoder active: d3d11va')).Count
    if ($connections -lt 2) { throw 'Hardware playback did not resume after server restart' }
    if ($fresh -notmatch 'SMOKE: connected=true, hardware_decode=true, frame_seen=true') {
        throw 'Final hardware playback confirmation missing'
    }
    if ($fresh -notmatch 'MODAL PROBE: decoded=\d+ presented=\d+ passed=true') {
        throw 'Video stopped presenting inside nested window message loop'
    }
    if (!$serverTwo.WaitForExit(12000)) { throw 'Second server did not exit on schedule' }
    if ($serverTwo.ExitCode -ne 0) { throw "Second server failed: $($serverTwo.ExitCode)" }

    # Datagram path: discard every 20th video datagram and expect parity repair.
    $lossOffset = (Get-Item -LiteralPath $viewerLog).Length
    $serverThree = Start-Process -FilePath (Join-Path $distDir 'server.exe') `
        -ArgumentList '--worker','--smoke-seconds','24' -WindowStyle Hidden -PassThru
    $lossViewer = Start-Process -FilePath (Join-Path $distDir 'viewer.exe') `
        -ArgumentList '127.0.0.1','--view-only','--no-audio','--video-drop-every','20','--smoke-seconds','15' -WindowStyle Hidden -PassThru
    if (!$lossViewer.WaitForExit(34000)) { throw 'Loss-recovery viewer did not exit on schedule' }
    if ($lossViewer.ExitCode -ne 0) { throw "Loss-recovery session failed: $($lossViewer.ExitCode)" }
    if (!$serverThree.WaitForExit(20000)) { throw 'Loss-recovery server did not exit on schedule' }
    if ($serverThree.ExitCode -ne 0) { throw "Loss-recovery server failed: $($serverThree.ExitCode)" }
    $lossFresh = Read-Fresh ([int]$lossOffset)
    if ($lossFresh -notmatch 'UDP video: first video data') { throw 'UDP video never reached the decoder' }
    if ($lossFresh -notmatch 'udp_recovered=[1-9]\d*') { throw 'Parity repair was never exercised' }
    if ($lossFresh -notmatch 'udp_datagrams=[1-9]') { throw 'UDP datagram statistics missing' }
    if ($lossFresh -notmatch 'SMOKE: connected=true, hardware_decode=true, frame_seen=true') {
        throw 'Hardware playback failed with simulated datagram loss'
    }

    # Desktop audio: the viewer must receive the server's capture format and pace it.
    $audioOffset = (Get-Item -LiteralPath $viewerLog).Length
    $serverFour = Start-Process -FilePath (Join-Path $distDir 'server.exe') `
        -ArgumentList '--worker','--smoke-seconds','16' -WindowStyle Hidden -PassThru
    $audioViewer = Start-Process -FilePath (Join-Path $distDir 'viewer.exe') `
        -ArgumentList '127.0.0.1','--view-only','--smoke-seconds','8' -WindowStyle Hidden -PassThru
    if (!$audioViewer.WaitForExit(24000)) { throw 'Audio viewer did not exit on schedule' }
    if ($audioViewer.ExitCode -ne 0) { throw "Audio session failed: $($audioViewer.ExitCode)" }
    if (!$serverFour.WaitForExit(16000)) { throw 'Audio server did not exit on schedule' }
    if ($serverFour.ExitCode -ne 0) { throw "Audio server failed: $($serverFour.ExitCode)" }
    $audioFresh = Read-Fresh ([int]$audioOffset)
    if ($audioFresh -notmatch 'Audio stream: \d+ Hz, 2 ch from server') {
        throw 'The viewer did not receive the server audio format'
    }
    if ($audioFresh -notmatch 'audio_kbps=\d+') { throw 'Audio pacing statistics missing' }
    if ($audioFresh -notmatch 'SMOKE: connected=true, hardware_decode=true, frame_seen=true') {
        throw 'Video failed during the audio session'
    }
    Write-Host "PASS: $connections hardware-decoded sessions; reconnection; nested message-loop presentation; encoder job cleanup; UDP parity recovery at 5% loss; desktop audio format and pacing."
} finally {
    foreach ($process in @($viewer, $lossViewer, $audioViewer, $serverOne, $serverTwo, $serverThree, $serverFour)) {
        if ($process -and !$process.HasExited) { $process.Kill(); $process.WaitForExit() }
    }
}
