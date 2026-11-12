# Build 0.2.1 validation — 2026-11-12

The default remains 1920×1080 at 60 FPS, HEVC fixed QP 16, speed preset. Native Rust Desktop Duplication now samples at a fixed cadence and resends the last desktop image while idle. One capture can wait for the encoder; one decoded image can wait for presentation. Missed capture ticks are discarded rather than replayed in a burst. DPI awareness and an Output1 fallback cover the locally observed output-duplication compatibility cases.

- Release build and Clippy with warnings denied passed. Eight unit tests passed, including timer-stall recovery and SHA-256 file transfer with Unicode names and filename collisions.
- New native capture/encode/decode/render loopback test passed at approximately 59–61 FPS, with zero overwritten pending decoded frames in the sampled intervals. Hardware decode remained d3d11va. Capture source was 2560×1440, encoded stream 1920×1080.
- This native-capture timestamp run measured 47 ms median image age, 47–78 ms over 11 samples. It is not the earlier 31 ms capture path, and does not establish lower latency. It targets stable cadence first. The same approximate clock and scanout limitations below apply.
- Real TCP input integration passed: 10 intercepted keyboard events and 10 mouse events, including automatic releases after disconnect; zero rejected injections.
- Packaged restart smoke passed: two hardware-decoded sessions, reconnection after server restart, final presented-frame confirmation, and no surviving encoder child.
- File transfer passed locally against the actual Desktop destination with matching file hashes. A 76 MB server executable was also sent to the RX 580 host; the receiver acknowledged the SHA-256-verified file as server-0.2.1.exe. It was not executed automatically. Explorer drag dispatch remains unverified through the physical UI.
- The cursor fallback is implemented for hidden/headless host pointers; cursor bitmap serialization passed its unit test. Visual confirmation of the new fallback on the Windows 10 host remains pending.
- The previous 0.2.0 RX 580 host was measured across the LAN at approximately 31–34 encoded/decoded/displayed FPS with no pending-frame skips and little socket write blocking. Low static-scene Mbps did not demonstrate packet loss. After the host update, 40 one-second samples averaged 60.00 captured, 60.09 encoded, and 60.00 decoded FPS. Static-scene traffic was commonly 0.37–0.38 Mbps while capture and decoding remained at 60 FPS. Hardware encoding was AMF HEVC fixed QP 16; decoding was d3d11va. The original client still showed several brief presentation-only stalls, distinct from host or network throughput.

- Client presentation now also runs from a window timer during nested move/resize/system-menu message loops. A four-second nested-loop test across the real LAN decoded 240 frames and presented 235 (58.75 FPS during that synthetic modal interval); normal intervals afterward were approximately 60 FPS with zero skipped pending frames. The smoke test now includes this regression check. Closing the viewer also stops the outer render loop before it touches a destroyed window.

The earlier validation below records the previous implementation and historical comparisons.
# Earlier validation — 2026-11-12

Test machine: Windows x64, AMD Radeon RX 9060 XT, driver 32.0.31044.16, 2560×1440 desktop at 180 Hz. Rust 1.98.1. FFmpeg 9.0.1. Revised target stream: 1920×1080, 60 FPS, HEVC at an 80 Mbps target, native Direct3D11 viewer. The original libmpv path was retained for diagnostic comparisons.

| Check | Result |
| --- | --- |
| Release compilation | Passed; two self-contained x64 Windows executables |
| `cargo clippy --all-targets --locked -- -D warnings` | Passed |
| Unit tests | Five passed: input serialization, malformed framing, coordinate mapping, native cursor bitmap roundtrip, timestamp barcode roundtrip/corruption in BGRA and limited-range luma |
| Packaged executable first launch | Embedded runtimes extracted into the content-addressed local cache and loaded successfully |
| Capture/scaling/encoding | Desktop Duplication capture with readback, then AMF GPU scaling/conversion/encoding |
| Direct GPU texture path | Driver rejected it; automatic fallback succeeded, recorded in `server.log` |
| Sustained local video | Approximately 59–60 encoded FPS in local runs; stream and decoder both reported 1920×1080 at 60 FPS |
| Viewer hardware decode/render | `d3d11va` hardware surfaces downloaded to NV12; native Direct3D11 shader converted/presented the image; windowed and fullscreen output visually inspected |
| H.264 compatibility option | Packaged `server.exe --codec h264` plus the native viewer passed hardware playback smoke test |
| Automatic discovery | Found the server on the active LAN interface without entering an IP |
| Server restart | Packaged smoke test passed with two independent hardware-decoded sessions and automatic reconnection |
| Encoder process cleanup | Windows job object terminated the encoder on server exit; no leftover test encoder |
| Real TCP input → SendInput | Passed: 10 keyboard hook events and 10 mouse hook events; zero rejected injections |
| Input details | A press/release, extended right Ctrl, numpad Enter, Pause, absolute/relative mouse, left/X buttons, vertical/horizontal wheel |
| Disconnect while inputs held | Server generated Shift-up and right-button-up automatically |

## Local latency comparison

A small topmost clock window painted a binary timestamp and checksum into the captured desktop. The source clock travelled through the actual capture, scaling, hardware encoder, TCP stream, hardware decoder, and viewer. The original player's current frame was read through its screenshot API; the native viewer read the luma bytes of its last GPU-submitted frame. No screenshot files were saved.

| Path | Samples | Minimum | Median | Maximum |
| --- | ---: | ---: | ---: | ---: |
| Original H.264/MPEG-TS/libmpv | 9 | 187 ms | 204 ms | 235 ms |
| Revised HEVC/NUT/native Direct3D, initial run | 9 | 32 ms | 32 ms | 47 ms |
| Packaged revised build, longer run | 23 | 16 ms | 31 ms | 63 ms |

The old screenshot API added 0–31 ms of sample readback overhead in that run; the native measurement added no full-frame readback beyond the normal display pipeline. The clock is approximately 16 ms-granular and desktop composition also affects the timestamp. These values are **approximate frame ages, not glass-to-glass or input-to-photon latency**. The longer run included a fullscreen transition. Both executables ran on this one PC; this is not a measurement across two physical LAN hosts.

Stage isolation also measured about 31 ms median for capture/scaling and 78 ms for the earlier hardware-decoder pipeline, which helped identify the avoidable playback scheduling/stream-framing delay. Merely changing H.264 to HEVC retained roughly 200 ms in the earlier media-player path; the native presentation path is the substantial change. The final decoder also retains the first probed keyframe, avoiding the earlier roughly one-second keyframe wait on initial playback.

The native input integration test installed hooks that intercepted only Screener-tagged test injections. Test keystrokes and clicks were swallowed before reaching the user's foreground application.

Two separate PCs across a physical gigabit link, NVIDIA/Intel hardware, worst-case game scenes, full input-to-display latency, and objective image-quality scores have **not** been measured. HEVC/H.264 4:2:0 is lossy. The implementation does not establish “zero loss,” “zero latency,” or universal game-input compatibility.

Reproduce with `scripts/build.ps1`, `scripts/smoke.ps1`, and the opt-in `tests/input_integration.rs` test. All test server/viewer processes were stopped after validation.
