# Screener

Remote desktop for Windows over a wired LAN, written in Rust. Screener uses hardware video encoding and decoding, with a native Direct3D 11 viewer. The default stream is 1920×1080 at 60 FPS using H.265/HEVC. Video travels over UDP with XOR parity repair; desktop audio, control, and file transfer stay on TCP.

## Features

- Automatic server discovery and reconnection.
- Hardware encoding through AMD AMF, NVIDIA NVENC, or Intel QSV. AMD is the tested backend.
- Video over UDP in MPEG-TS-aligned shards with one parity datagram per group, so a single lost datagram is repaired without a retransmit.
- Runs as a LocalSystem Windows service that starts the server in the signed-in user's session. This is what allows the viewer to see and answer UAC prompts, the lock screen, and other secure-desktop dialogs.
- Keyboard input, five-button mouse support, both scroll wheels, and relative mouse mode for games.
- Local cursor rendering, including a fallback for hosts without a visible pointer.
- Drag-and-drop file transfer to the signed-in user's Desktop, with SHA-256 verification and no overwriting of existing files.
- Desktop audio captured with WASAPI loopback from the default output device and played through the viewer's default output device, with no virtual audio driver.
- Fixed capture cadence while idle, with stale pending frames replaced instead of queued for playback.

## Getting started

Build the project using the instructions below. The output is two standalone executables in `dist/`; each includes its video runtime.

1. Launch `server.exe` on the computer you want to control. Windows shows one UAC consent prompt: the server copies itself to `C:\Program Files\Screener`, adds Windows Firewall rules for the local subnet on the Private network, installs an automatic-start LocalSystem service, and the service starts the server in your session. UAC itself stays enabled. Logs are written in `C:\Program Files\Screener`.
2. Launch `viewer.exe` on the other computer. It finds the server on the same subnet and connects.
3. Drop files from Explorer onto the viewer to copy them to the remote Desktop. Transfer progress appears in the window title. Desktop audio plays through the viewer's default output device.

To choose a server explicitly:

```powershell
.\viewer.exe 192.168.1.20
```

Changing server options (for example `--width 1280`) updates the installed service and asks for administrator approval once, because the service configuration is protected. The options are remembered and reused at every boot. `--probe` and `--smoke-seconds` run in the current process instead of the service.

To remove the service and its install directory:

```powershell
.\server.exe --uninstall-service
```

`server.exe --service-status` reports the installed service state without elevation. `server.exe --worker` runs the server directly in the current process for diagnostics; it is not the normal operating mode.

The first launch extracts the bundled runtime beside the executable; the service version keeps it in `C:\Program Files\Screener\runtime`. Logs are written beside the executable, so use a writable directory for direct runs. Stop or remove the service through the commands above or Services. Keep both executables on the same version when updating: launching a rebuilt `server.exe` updates the installed copy automatically.

Screener has no authentication or encryption. Use it on a trusted private LAN and do not expose its ports to the internet. Only one viewer/controller is supported at a time.

### Network setup

Allow these inbound ports on the server:

| Port | Protocol | Purpose |
| --- | --- | --- |
| 47990 | UDP | Video (MPEG-TS shards with XOR parity) |
| 47990 | TCP | Video over TCP, only for the `--legacy-buffering` diagnostic |
| 47991 | TCP | Keyboard, mouse, and status |
| 47992 | UDP | Discovery |
| 47993 | TCP | File transfer |

The service installer adds these rules for `C:\Program Files\Screener\server.exe` on the Private profile, limited to the local subnet; `--no-firewall` skips that step. Use `dist/allow-lan.ps1` from an elevated PowerShell when the network is classified Public or Domain, when the rules were skipped, or when running a portable `--worker` copy. A third-party firewall may still block the ports; allow them there as well.

### Controls

| Shortcut | Action |
| --- | --- |
| `F8` | Switch between desktop and relative mouse modes |
| `F11` | Toggle fullscreen |
| `Ctrl+Alt+F12` | Release remote input and mouse confinement |

Click the viewer to take control again after releasing input. Losing focus or disconnecting releases held keys and mouse buttons.

## Configuration

The default server settings are equivalent to:

```powershell
.\server.exe --width 1920 --height 1080 --fps 60 --codec hevc --rate-control quality --qp 16 --preset speed
```

Other common options:

```powershell
# H.264 instead of HEVC
.\server.exe --codec h264

# Target an 80 Mbps bitrate instead of fixed quality
.\server.exe --rate-control cbr --mbps 80

# Select another output on the first GPU
.\server.exe --monitor 1

# Fullscreen viewer with relative mouse input
.\viewer.exe 192.168.1.20 --fullscreen --relative

# Watch without forwarding input
.\viewer.exe 192.168.1.20 --view-only

# Send a file without opening the viewer window
.\viewer.exe 192.168.1.20 --send "C:\Documents\report.pdf"

# Watch without audio, or with a larger audio playout buffer
.\viewer.exe 192.168.1.20 --no-audio
.\viewer.exe 192.168.1.20 --audio-buffer-ms 80

# Stop capturing desktop audio on the server
.\server.exe --no-audio
```

Quality mode uses a fixed quantizer: lower `--qp` values preserve more detail and use more bandwidth. `--mbps` applies only to CBR mode. Low bitrate on a static desktop is expected and does not reduce the configured capture cadence.

Desktop audio is on by default and uses no additional ports: it shares the control connection. The viewer asks the server for audio unless `--no-audio` is given, and `--no-audio` on the server disables capture for every viewer. `--audio-buffer-ms` (10–500, default 50) trades audio latency for tolerance of network jitter. Audio is not volume-controlled by Screener; it follows both machines' system volumes.

Use `--encoder amf`, `nvenc`, or `qsv` to override vendor detection. Custom ports are set with `--port`, `--input-port`, and `--file-port`. The default bind address is `0.0.0.0`. Options are stored with the installed service and apply to every restart; running `server.exe` with different options updates the service.

## Build

Requires Windows x64, Rust with the MSVC toolchain and C++ build tools, PowerShell, and `tar` for runtime extraction.

```powershell
.\scripts\fetch-runtime.ps1
.\scripts\build.ps1
```

The fetch script downloads pinned FFmpeg and libmpv builds and verifies their hashes. The build embeds compressed runtimes in `dist/server.exe` and `dist/viewer.exe`. Generated binaries and downloaded runtimes are not tracked in Git.

### Checks

```powershell
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
.\scripts\smoke.ps1
```

The smoke test requires a capture-capable desktop, a supported GPU, and an audio output device. It checks hardware playback, reconnection after a server restart, presentation during a nested window message loop, encoder process cleanup, a UDP session with simulated datagram loss repaired by parity, and desktop audio format delivery and pacing.

The optional input integration test intercepts its own injected events:

```powershell
cargo test --test input_integration --locked -- --ignored --nocapture
```

## How it works

The server starts as a LocalSystem service. The service watches the active console session and starts a worker there with a duplicated SYSTEM token, on `winsta0\default`. The worker samples the desktop through Direct3D Desktop Duplication and feeds a hardware encoder. Idle images are repeated at the selected cadence. Missed capture ticks are discarded rather than sent in a catch-up burst.

Video is MPEG-TS split into 188-byte-aligned shards: four shards plus one XOR parity datagram per group, with a 10 ms reorder deadline. One lost datagram per group is reconstructed without retransmission; data that stays missing is skipped so the demuxer can resynchronize. Control and file transfers use separate TCP connections.

The capture and input threads follow whichever desktop is receiving input. When Windows switches to the Winlogon secure desktop for a UAC prompt or the lock screen, the LocalSystem worker reattaches its duplication and injection there, so the viewer can see and answer the prompt. Input injection runs on a dedicated thread with no windows, hooks, or DXGI objects of its own, and keeps the desktop handle open while attached; on the normal desktop this adds only a cheap desktop check per input event.

Audio is captured from the default output device with WASAPI loopback, so it is exactly what the server user hears: no virtual cable or "Stereo Mix" device is needed. Device float or 16/24/32-bit samples are converted to 16-bit stereo at the device rate and sent as length-prefixed PCM on the control connection. The viewer opens its default output device with WASAPI, lets the audio engine resample if the two devices run at different rates, and keeps a small playout buffer (`--audio-buffer-ms`) between the network and the speaker. Silent capture packets are not transmitted, so an idle desktop costs no audio bandwidth.

The viewer uses D3D11 hardware decoding, NV12 readback, and GPU color conversion and presentation. Capture and display each retain at most one pending frame. The current path includes GPU readback on both sides; it is not zero-copy. File transfer is limited to 15 MiB/s to leave bandwidth for video.

Tested hardware includes an RX 580 host on Windows 10 and an RX 9060 XT viewer. See [VALIDATION.md](VALIDATION.md) for measurements, test conditions, and known gaps. HEVC and H.264 are lossy; end-to-end input-to-display latency has not been measured across two physical machines.

## Limitations and diagnostics

Capture supports one selected, unrotated output on DXGI adapter zero. Use an SDR desktop. Audio is uncompressed 16-bit stereo at the capture device's rate (about 1.5 Mbps at 48 kHz), taken from the default output device and mixed down to the front pair on multichannel endpoints; there is no codec, volume control, or A/V delay compensation beyond the playout buffer. The viewer plays it on its default output device. Clipboard synchronization, folder transfers, reverse file transfers, and startup at boot for direct (non-service) runs are not implemented. Ctrl+Alt+Delete is not forwarded: the security screen requires `SendSAS`, which is deliberately not implemented. UAC prompts and the lock screen can be captured and answered only when the service is installed; a direct `--worker` run cannot open the secure desktop.

Diagnostic logs are `server.log`, `service.log`, `encoder.log`, `viewer.log`, and `decoder.log`. The service and session worker write into `C:\Program Files\Screener`; a direct `--worker` run writes beside its executable. The viewer's `PERF` lines separate capture, encoder submission, encoding, decoding, and presentation rates. They also report skipped pending frames, bitrate, socket-write wait time, UDP datagram, parity-recovery, and missing-shard counters, and audio bitrate, peak level, underruns, and dropped chunks. Input logs record event counts, not typed text.

## License

[GPL-3.0-or-later](LICENSE). Bundled video engines and their source/build references are listed in [THIRD_PARTY.md](THIRD_PARTY.md).
