# Screener

Remote desktop for Windows over a wired LAN, written in Rust. Screener uses hardware video encoding and decoding, with a native Direct3D 11 viewer. The default stream is 1920×1080 at 60 FPS using H.265/HEVC.

## Features

- Automatic server discovery and reconnection.
- Hardware encoding through AMD AMF, NVIDIA NVENC, or Intel QSV. AMD is the tested backend.
- Keyboard input, five-button mouse support, both scroll wheels, and relative mouse mode for games.
- Local cursor rendering, including a fallback for hosts without a visible pointer.
- Drag-and-drop file transfer to the server user's Desktop, with SHA-256 verification and no overwriting of existing files.
- Fixed capture cadence while idle, with stale pending frames replaced instead of queued for playback.

## Getting started

Build the project using the instructions below. The output is two standalone executables in `dist/`; each includes its video runtime.

1. Launch `server.exe` on the computer you want to control. It runs in the background and starts listening immediately.
2. Launch `viewer.exe` on the other computer. It finds the server on the same subnet and connects.
3. Drop files from Explorer onto the viewer to copy them to the remote Desktop. Transfer progress appears in the window title.

To choose a server explicitly:

```powershell
.\viewer.exe 192.168.1.20
```

The first launch extracts the bundled runtime into `%LOCALAPPDATA%\Screener\runtime`. Logs are written beside the executables, so use a writable directory. Stop the server through Task Manager. Keep both executables on the same version when updating.

Screener has no authentication or encryption. Use it on a trusted private LAN and do not expose its ports to the internet. Only one viewer/controller is supported at a time.

### Network setup

Allow these inbound ports on the server:

| Port | Protocol | Purpose |
| --- | --- | --- |
| 47990 | TCP | Video |
| 47991 | TCP | Keyboard, mouse, and status |
| 47992 | UDP | Discovery |
| 47993 | TCP | File transfer |

The optional `dist/allow-lan.ps1` script adds Windows Firewall rules for the local subnet on a Private network. Run it once from an elevated PowerShell. Screener does not change firewall settings itself; Windows may show its own firewall prompt.

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
```

Quality mode uses a fixed quantizer: lower `--qp` values preserve more detail and use more bandwidth. `--mbps` applies only to CBR mode. Low bitrate on a static desktop is expected and does not reduce the configured capture cadence.

Use `--encoder amf`, `nvenc`, or `qsv` to override vendor detection. Custom ports are set with `--port`, `--input-port`, and `--file-port`. The default bind address is `0.0.0.0`.

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

The smoke test requires a capture-capable desktop and supported GPU. It checks hardware playback, reconnection after a server restart, presentation during a nested window message loop, and encoder process cleanup.

The optional input integration test intercepts its own injected events:

```powershell
cargo test --test input_integration --locked -- --ignored --nocapture
```

## How it works

The server samples the desktop through Direct3D Desktop Duplication and feeds a hardware encoder. Idle images are repeated at the selected cadence. Missed capture ticks are discarded rather than sent in a catch-up burst. Video is carried as flushed NUT over TCP; input and file transfers use separate connections.

The viewer uses D3D11 hardware decoding, NV12 readback, and GPU color conversion and presentation. Capture and display each retain at most one pending frame. The current path includes GPU readback on both sides; it is not zero-copy. File transfer is limited to 15 MiB/s to leave bandwidth for video.

Tested hardware includes an RX 580 host on Windows 10 and an RX 9060 XT viewer. See [VALIDATION.md](VALIDATION.md) for measurements, test conditions, and known gaps. HEVC and H.264 are lossy; end-to-end input-to-display latency has not been measured.

## Limitations and diagnostics

Capture supports one selected, unrotated output on DXGI adapter zero. Use an SDR desktop. Audio, clipboard synchronization, folder transfers, reverse file transfers, and startup at boot are not implemented. Secure desktops, Ctrl+Alt+Delete, protected video, and applications that reject injected input are outside the normal capture/input path. Elevated applications require a server running at a matching privilege level.

Diagnostic logs are `server.log`, `encoder.log`, `viewer.log`, and `decoder.log`. The viewer's `PERF` lines separate capture, encoder submission, encoding, decoding, and presentation rates. They also report skipped pending frames, bitrate, and socket-write wait time. Input logs record event counts, not typed text.

## License

[GPL-3.0-or-later](LICENSE). Bundled video engines and their source/build references are listed in [THIRD_PARTY.md](THIRD_PARTY.md).
