use crate::{
    config::{CaptureMode, Encoder, ServerConfig},
    discovery,
    input::{current_cursor, DesktopRect, Injector},
    media::{self, ManagedChild},
    protocol::{write_packet, InputEvent, INPUT_SIZE, MAGIC},
    telemetry::{ServerInfo, StreamCounters},
    util::{log, private_peer},
};
use anyhow::{bail, ensure, Context, Result};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::windows::io::{AsHandle, AsRawHandle},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::{Foundation::HANDLE, System::Threading::TerminateProcess};

pub fn display_info(index: u32) -> Result<(Encoder, DesktopRect)> {
    display_info_impl(index, true)
}
pub fn display_info_quiet(index: u32) -> Result<(Encoder, DesktopRect)> {
    display_info_impl(index, false)
}
fn display_info_impl(index: u32, verbose: bool) -> Result<(Encoder, DesktopRect)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter = factory.EnumAdapters1(0)?;
        let desc = adapter.GetDesc1()?;
        let name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .to_string();
        let out = adapter.EnumOutputs(index)?.GetDesc()?;
        ensure!(
            out.AttachedToDesktop.as_bool(),
            "monitor is not attached to the desktop"
        );
        ensure!(
            out.Rotation == windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY,
            "rotated monitors are not supported yet"
        );
        if verbose {
            log(format!(
                "Adapter {name}; output {index}; desktop {:?}",
                out.DesktopCoordinates
            ));
        }
        let enc = match desc.VendorId {
            0x1002 => Encoder::Amf,
            0x10de => Encoder::Nvenc,
            0x8086 => Encoder::Qsv,
            _ => Encoder::Auto,
        };
        let r = out.DesktopCoordinates;
        Ok((
            enc,
            DesktopRect {
                left: r.left,
                top: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
            },
        ))
    }
}

pub fn run(c: ServerConfig) -> Result<()> {
    ensure!(
        c.width.is_multiple_of(2) && c.height.is_multiple_of(2),
        "4:2:0 video needs even dimensions"
    );
    ensure!(
        c.port != c.input_port && c.file_port != c.port && c.file_port != c.input_port,
        "video, input and file ports must differ"
    );
    let (enc, rect) = display_info(c.monitor)?;
    let pipeline = media::choose_pipeline(&c, enc)?;
    if c.probe {
        let mut cmd = media::ffmpeg_command(&c, &pipeline)?;
        cmd.args(["-t", "5", "-benchmark", "-f", "null", "-"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(media::encoder_log()?));
        let start = Instant::now();
        let mut child = ManagedChild::spawn(&mut cmd)?;
        let mut feed = if c.capture == CaptureMode::Native {
            Some(crate::capture::Feed::start(
                child.child.stdin.take().context("capture pipe")?,
                c.monitor,
                c.fps,
                Arc::new(StreamCounters::default()),
            ))
        } else {
            None
        };
        let status = child.child.wait()?;
        if let Some(feed) = feed.as_mut() {
            feed.stop();
        }
        ensure!(status.success(), "hardware benchmark failed");
        log(format!(
            "PROBE OK: {}x{} @ {}fps, {} Mbps, {} in {:?}",
            c.width,
            c.height,
            c.fps,
            c.mbps,
            pipeline.label,
            start.elapsed()
        ));
        return Ok(());
    }
    let video = TcpListener::bind((c.bind.as_str(), c.port)).context("binding video port")?;
    let control =
        TcpListener::bind((c.bind.as_str(), c.input_port)).context("binding input port")?;
    video.set_nonblocking(true)?;
    control.set_nonblocking(true)?;
    let alive = Arc::new(AtomicBool::new(true));
    crate::files::listen(&c.bind, c.file_port, alive.clone())?;
    let counters = Arc::new(StreamCounters::default());
    counters.reset();
    let info = Arc::new(server_info(&c, &pipeline)?);
    discovery::serve(&c.bind, c.port, c.input_port, alive.clone())?;
    let stop = alive.clone();
    let start = Instant::now();
    let cfg = Arc::new(c);
    let input_cfg = cfg.clone();
    let input_alive = alive.clone();
    let input_counters = counters.clone();
    let input_info = info.clone();
    std::thread::spawn(move || {
        while input_alive.load(Ordering::Relaxed) {
            match control.accept() {
                Ok((stream, peer)) if private_peer(peer.ip()) => {
                    // One controller at a time; serve it synchronously so two keyboards cannot conflict.
                    if let Err(e) = control_session(
                        stream,
                        rect,
                        &input_cfg,
                        &input_alive,
                        &input_info,
                        input_counters.clone(),
                    ) {
                        log(format!("Input session {peer}: {e:#}"));
                    }
                }
                Ok((_s, _p)) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(e) => {
                    log(format!("Input listener: {e}"));
                    break;
                }
            }
        }
    });
    let busy = Arc::new(AtomicBool::new(false));
    log(format!(
        "Listening: video TCP {}, input TCP {}, files TCP {}, discovery UDP 47992; {}x{} @ {}fps; {:?} {:?} QP {} preset {:?}",
        cfg.port, cfg.input_port,cfg.file_port,cfg.width,cfg.height,cfg.fps,cfg.codec,cfg.rate_control,cfg.qp,cfg.preset
    ));
    while alive.load(Ordering::Relaxed) {
        if cfg
            .smoke_seconds
            .is_some_and(|s| start.elapsed().as_secs() >= s)
        {
            break;
        }
        match video.accept() {
            Ok((stream, peer)) if private_peer(peer.ip()) => {
                if busy.swap(true, Ordering::SeqCst) {
                    continue;
                }
                let busy = busy.clone();
                let cfg = cfg.clone();
                let pipeline = pipeline.clone();
                let alive = alive.clone();
                let counters = counters.clone();
                std::thread::spawn(move || {
                    log(format!("Video connected: {peer}"));
                    if let Err(e) = video_session(stream, &cfg, &pipeline, &alive, counters) {
                        log(format!("Video session: {e:#}"));
                    }
                    busy.store(false, Ordering::SeqCst);
                    log("Video disconnected; ready for the next viewer");
                });
            }
            Ok((_s, _p)) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) => bail!("video listener: {e}"),
        }
    }
    stop.store(false, Ordering::Relaxed);
    Ok(())
}

fn control_session(
    mut stream: TcpStream,
    rect: DesktopRect,
    c: &ServerConfig,
    alive: &AtomicBool,
    info: &ServerInfo,
    counters: Arc<StreamCounters>,
) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let mut magic = [0; 8];
    stream.read_exact(&mut magic)?;
    ensure!(&magic == MAGIC, "protocol mismatch");
    // Fixed 36-byte handshake: magic, desktop rectangle, video geometry, fps, ports.
    let mut hello = MAGIC.to_vec();
    for v in [rect.left, rect.top, rect.width, rect.height] {
        hello.extend_from_slice(&v.to_le_bytes());
    }
    for v in [c.width, c.height, c.fps, c.port, c.input_port, 1] {
        hello.extend_from_slice(&v.to_le_bytes());
    }
    stream.write_all(&hello)?;
    stream.write_all(&[0xC2, 0])?;
    write_packet(&mut stream, &serde_json::to_vec(info)?)?;
    let mut injector = Injector::new(rect);
    let mut status = stream.try_clone()?;
    let connected = Arc::new(AtomicBool::new(true));
    let status_connected = connected.clone();
    let status_thread = std::thread::spawn(move || {
        let mut last_handle = 0;
        let mut last_stats = Instant::now();
        while status_connected.load(Ordering::Relaxed) {
            let (id, handle) = current_cursor();
            if id == 255 && handle != last_handle {
                if let Ok(shape) = crate::cursor::snapshot(handle) {
                    let mut packet = vec![];
                    shape.encode(&mut packet);
                    if status.write_all(&[0xC1, 0]).is_err()
                        || write_packet(&mut status, &packet).is_err()
                    {
                        break;
                    }
                    last_handle = handle;
                }
            } else if id != 255 {
                last_handle = 0;
            }
            // Send a heartbeat even when the cursor shape has not changed.
            let record = [0xC0, id];
            if status.write_all(&record).is_err() {
                break;
            }
            if last_stats.elapsed() >= Duration::from_secs(1) {
                let packet = serde_json::to_vec(&counters.snapshot()).unwrap_or_default();
                if status.write_all(&[0xC3, 0]).is_err()
                    || write_packet(&mut status, &packet).is_err()
                {
                    break;
                }
                last_stats = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(33));
        }
        let _ = status.shutdown(std::net::Shutdown::Both);
    });
    let result = (|| -> Result<()> {
        let mut buf = [0; INPUT_SIZE];
        let mut last_error = Instant::now() - Duration::from_secs(10);
        let (mut keyboard, mut mouse, mut rejected) = (0u64, 0u64, 0u64);
        let mut refresh = Instant::now();
        let output = unsafe {
            CreateDXGIFactory1::<IDXGIFactory1>()?
                .EnumAdapters1(0)?
                .EnumOutputs(c.monitor)?
        };
        while alive.load(Ordering::Relaxed) {
            if let Err(e) = stream.read_exact(&mut buf) {
                log(format!("Input summary: {keyboard} keyboard events, {mouse} mouse events, {rejected} rejected injections"));
                return Err(e.into());
            }
            let event = InputEvent::decode(&buf)?;
            match event {
                InputEvent::Key { .. } => keyboard += 1,
                InputEvent::Absolute { .. }
                | InputEvent::Relative { .. }
                | InputEvent::Button { .. }
                | InputEvent::Wheel { .. } => mouse += 1,
                _ => {}
            }
            if refresh.elapsed() > Duration::from_secs(1) {
                if let Ok(info) = unsafe { output.GetDesc() } {
                    let r = info.DesktopCoordinates;
                    if r.right > r.left && r.bottom > r.top {
                        injector.rect = DesktopRect {
                            left: r.left,
                            top: r.top,
                            width: r.right - r.left,
                            height: r.bottom - r.top,
                        };
                    }
                }
                refresh = Instant::now();
            }
            if let Err(e) = injector.apply(event) {
                rejected += 1;
                if last_error.elapsed() > Duration::from_secs(2) {
                    log(e);
                    last_error = Instant::now();
                }
            }
        }
        Ok(())
    })();
    connected.store(false, Ordering::Relaxed);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = status_thread.join();
    result
}

fn video_session(
    mut stream: TcpStream,
    c: &ServerConfig,
    p: &media::Pipeline,
    alive: &AtomicBool,
    counters: Arc<StreamCounters>,
) -> Result<()> {
    stream.set_nonblocking(false)?;
    // Keep a stalled receiver from accumulating a large queue of old frames.
    socket2::SockRef::from(&stream).set_send_buffer_size(64 * 1024)?;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    let mut cmd = media::ffmpeg_command(c, p)?;
    counters.reset();
    cmd.args([
        "-f",
        "nut",
        "-write_index",
        "0",
        "-flush_packets",
        "1",
        "pipe:1",
        "-progress",
        "pipe:2",
        "-stats_period",
        "1",
    ])
    .stderr(Stdio::piped());
    let mut child = ManagedChild::spawn(&mut cmd)?;
    let mut capture_feed = if c.capture == CaptureMode::Native {
        Some(crate::capture::Feed::start(
            child.child.stdin.take().context("capture pipe")?,
            c.monitor,
            c.fps,
            counters.clone(),
        ))
    } else {
        None
    };
    // A blocked encoder used to keep `busy` true forever because the stdout read
    // could not notice that the viewer had disconnected. Watch both independently.
    let process = child.child.as_handle().try_clone_to_owned()?;
    let mut peer_watch = stream.try_clone()?;
    peer_watch.set_read_timeout(Some(Duration::from_millis(250)))?;
    let watching = Arc::new(AtomicBool::new(true));
    let watch_active = watching.clone();
    let last_output = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let observed_output = last_output.clone();
    let started = Instant::now();
    let watchdog = std::thread::spawn(move || {
        let mut byte = [0];
        while watch_active.load(Ordering::Relaxed) {
            let failure = match peer_watch.read(&mut byte) {
                Ok(0) => Some("viewer disconnected"),
                Ok(_) => None,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    None
                }
                Err(_) => Some("video socket closed"),
            };
            let elapsed = started.elapsed().as_millis() as u64;
            let stalled = elapsed > 8000
                && elapsed.saturating_sub(observed_output.load(Ordering::Relaxed)) > 5000;
            if (failure.is_some() || stalled) && watch_active.load(Ordering::Relaxed) {
                log(format!(
                    "Stopping encoder: {}",
                    failure.unwrap_or("no video output for five seconds")
                ));
                unsafe {
                    let _ = TerminateProcess(HANDLE(process.as_raw_handle()), 1);
                }
                break;
            }
        }
    });
    let errors = child.child.stderr.take().context("encoder diagnostics")?;
    let progress_counters = counters.clone();
    std::thread::spawn(move || {
        let mut log_file = media::encoder_log().ok();
        for line in BufReader::new(errors).lines() {
            let Ok(line) = line else {
                break;
            };
            if let Some(value) = line.strip_prefix("frame=") {
                if let Ok(count) = value.trim().parse::<u64>() {
                    progress_counters.encoded.store(count, Ordering::Relaxed);
                }
            }
            if let Some(file) = log_file.as_mut() {
                let _ = writeln!(file, "{line}");
            }
        }
    });
    let mut output = child.child.stdout.take().context("encoder stdout")?;
    let mut buf = [0; 32 * 1024];
    let start = Instant::now();
    let mut bytes = 0u64;
    let result = (|| -> Result<()> {
        while alive.load(Ordering::Relaxed) {
            let n = output.read(&mut buf)?;
            if n == 0 {
                break;
            }
            last_output.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            let sent_at = Instant::now();
            stream.write_all(&buf[..n])?;
            counters
                .send_wait_us
                .fetch_add(sent_at.elapsed().as_micros() as u64, Ordering::Relaxed);
            counters.bytes.fetch_add(n as u64, Ordering::Relaxed);
            bytes += n as u64;
        }
        Ok(())
    })();
    watching.store(false, Ordering::Relaxed);
    let _ = child.child.kill();
    if let Some(feed) = capture_feed.as_mut() {
        feed.stop();
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = watchdog.join();
    log(format!("Sent {bytes} bytes in {:?}", start.elapsed()));
    result
}

fn server_info(c: &ServerConfig, p: &media::Pipeline) -> Result<ServerInfo> {
    let adapter = unsafe {
        CreateDXGIFactory1::<IDXGIFactory1>()?
            .EnumAdapters1(0)?
            .GetDesc1()?
    };
    Ok(ServerInfo {
        build: env!("CARGO_PKG_VERSION").into(),
        machine: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows host".into()),
        adapter: String::from_utf16_lossy(&adapter.Description)
            .trim_end_matches('\0')
            .into(),
        encoder: format!("{:?}", p.encoder),
        codec: format!("{:?}", c.codec),
        pipeline: format!("{:?} capture; {}", c.capture, p.label),
        rate_control: format!("{:?}", c.rate_control),
        qp: c.qp,
        file_port: c.file_port,
        target_fps: c.fps,
    })
}
