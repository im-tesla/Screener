# Build 0.3.2 validation — 2026-11-12

The default video path is now UDP: MPEG-TS split into 188-byte-aligned shards, four shards plus one XOR parity datagram per group, a 10 ms reorder deadline, and a reorder window that skips ahead instead of rejecting everything after a stall. Control and files remain on TCP. The server is installed as an automatic-start LocalSystem service and runs its worker in the signed-in user's session. Capture and input threads follow the active desktop. Secure-desktop capture is confirmed in the field (the UAC prompt is visible through the viewer); secure-desktop input is the issue this build addresses.

- Release build, `cargo fmt --check`, 18 unit tests (plus an opt-in WASAPI probe), and Clippy with warnings denied passed.
- The service installer now adds Private-profile, local-subnet inbound rules for the installed executable (UDP 47990 and 47992, TCP 47990, 47991, and 47993) and replaces them by name on reinstall; `--no-firewall` skips this. The `netsh` argument form was validated without elevation (it reported that elevation is required and added nothing). The elevated add itself was not executed here. A real deployment failure matched this gap: the installed worker answered UDP discovery, but TCP 47990/47991 timed out from another LAN host while SMB and RPC were reachable.
- UDP loopback, one PC: hardware d3d11va decoding sustained 59–61 decoded/displayed FPS at `--rate-control cbr --mbps 80`, and at approximately 85–126 Mbps in quality mode on a busy desktop. First video data arrived about 320–360 ms after connect.
- Simulated datagram loss: `--video-drop-every 20` (5% of datagrams discarded before recovery). Over 12 s at 60 FPS the viewer recovered 236 datagrams by parity with `missing_shards=0` after startup; no decoder errors were logged. At about 100 Mbps and 5% loss, 1836 datagrams were repaired with no missing shards after startup.
- Startup gap: at high bitrate the decoder starts while the sender is already streaming, so the first second can skip roughly 1 MB (about 239 shards) before the stream resynchronizes. Playback then held 60 FPS. This is recorded as an expected cost of datagram video, not hidden.
- The nested message-loop presentation check passed at 193–200 of 241 decoded frames (80–83%). The earlier TCP NUT path reached 235 of 240 (98%); datagram delivery is burstier, so a few frames are replaced before the nested-loop timer runs. The pass threshold is 75% so a frozen video still fails while normal jitter passes. The normal presentation loop showed zero skipped pending frames at 59–61 FPS.
- The packaged smoke test passed on the release binaries: two hardware-decoded sessions, automatic reconnection after a server restart, presentation through a nested window message loop, encoder child cleanup, a third session with 5% simulated datagram loss in which parity repaired the loss and hardware playback stayed connected, and a fourth session in which the viewer received the server audio format and reported pacing statistics.
- A later local smoke attempt could not capture: the desktop had switched to a 10-bit (DXGI_FORMAT_R10G10B10A2) surface, Desktop Duplication returned 10-bit textures, and the documented SDR-only limitation applied. The same smoke passed earlier in this session; the changes in this build do not touch the capture format. Use an 8-bit SDR desktop for capture.
- Field `server.log` showed `Desktop switch: thread N -> Winlogon` followed by `SendInput rejected an event on desktop 'Winlogon': Access is denied (5)`, and the same rejection persisted after switching back to Default until the service restarted. Root cause: the desktop handles were opened with a read/write/switch/enumerate mask. A thread attached with that mask cannot inject input — SendInput returns ERROR_ACCESS_DENIED even on the default desktop. Desktop attachment now requests GENERIC_ALL (the mask used by desktop-aware input injectors) and uses a fresh thread per desktop, attached before that thread makes any other User32 call; the handle stays open while attached.
- The real input integration test passes with the fix: 9 keyboard and 9 mouse events, 0 rejected injections, 0 dropped events, including disconnect releases. The earlier failure mode is reproduced and fixed deterministically on the normal desktop: with the restricted mask the same test reported 18 rejected injections.
- Secure-desktop input is still pending a field test against the installed service, but the failing path and the fix are the same attachment code that is now verified on Default. The server logs `Desktop attach: thread N -> Winlogon` and a detailed `SendInput rejected an event on desktop ...` line if injection still fails.
- Desktop audio is new in this build: WASAPI loopback capture from the server's default output device, 16-bit stereo PCM over the existing control TCP connection, and WASAPI rendering on the viewer's default output device. No extra port or codec is involved; bandwidth is about 1.5 Mbps at 48 kHz.
- This host's device mix was 32-bit float, 48 kHz, stereo. The server logged `Audio capture: 48000 Hz, 2 ch loopback`; the viewer logged `Audio renderer: 48000 Hz, 2 ch, 50 ms playout buffer`.
- The opt-in probe `cargo test --lib audio::tests::loopback_captures_rendered_tone -- --ignored` renders a short tone, captures it from the same default device, and requires both playback and a non-silent loopback chunk. It reported 23,040 bytes played and a captured peak of 32,767. It needs an active audio output device and is not part of the default test run.
- A five-second end-to-end session with a quiet 0.2 s test tone measured 1,524–1,536 kbps of audio, tracked the tone in the reported peak (0.04 rising to 0.36 as the single-PC loop repeated), and dropped no audio chunks; the six reported underruns occurred at the end of the tone.
- Single-PC audio tests feed the viewer's playback back into the server's loopback capture. That echo is a test artifact and does not occur between two machines.
- Audio **not** verified: audible lip sync, capture from the LocalSystem service worker (the service was not installed), 44.1 kHz and multichannel endpoint conversion, and behavior on the RX 580 host.
- Validation found and fixed two transport defects: a reorder window of 64 groups could wedge the receiver permanently when a decoder-start stall let the sender run farther ahead, and the server's UDP listener treated the ICMP reset from a departing viewer as fatal. The server now survives viewer disconnects.
- Service, interactive-session launch, and UAC prompt interaction were **not** executed on this machine: installing the service requires an interactive administrator consent that automated validation must not trigger. `server.exe --service` correctly reports that it must be started by the service manager, and `--service-status` reports installation state. Secure-desktop capture and injection therefore remain unverified in this build.
- This was loopback on one PC. Two-PC LAN behavior, physical UAC prompt interaction, the lock screen, and input-to-display latency remain unverified.

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
