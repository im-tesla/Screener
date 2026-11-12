use crate::{
    config::{CaptureMode, Encoder, ServerConfig},
    discovery,
    input::{current_cursor, DesktopRect, Injector},
    media::{self, ManagedChild},
    protocol::{write_packet, InputEvent, INPUT_SIZE, MAGIC},
    telemetry::{ServerInfo, StreamCounters},
    udp,
    util::{log, private_peer},
};
use anyhow::{bail, ensure, Context, Result};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    os::windows::io::{AsHandle, AsRawHandle},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::{Foundation::HANDLE, System::Threading::TerminateProcess};

enum Injection {
    Event(InputEvent),
    Rect(DesktopRect),
}

/// One input thread per desktop visit. A thread that already owns a window,
/// hook, or input queue does not retarget its input with SetThreadDesktop, so the
/// injector is recreated whenever the input desktop changes.
struct DesktopInjector {
    desktop: String,
    tx: crossbeam_channel::Sender<Injection>,
    thread: std::thread::JoinHandle<()>,
}

impl DesktopInjector {
    fn start(desktop: &str, rect: DesktopRect, rejected: Arc<AtomicU64>) -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<Injection>(512);
        let name = desktop.to_string();
        let worker = name.clone();
        let thread = std::thread::spawn(move || {
            // Attach first: this thread must have no window, hook, or input
            // queue before it joins the target desktop.
            if let Err(e) = crate::desktop::attach(&worker) {
                log(format!("Input injector for {worker}: {e:#}"));
                return;
            }
            let mut injector = Injector::new_pinned(rect);
            let mut last_log = Instant::now() - Duration::from_secs(10);
            log(format!(
                "Input injector thread started on the {worker} desktop"
            ));
            while let Ok(command) = rx.recv() {
                match command {
                    Injection::Rect(rect) => injector.rect = rect,
                    Injection::Event(event) => {
                        if let Err(e) = injector.apply(event) {
                            rejected.fetch_add(1, Ordering::Relaxed);
                            if last_log.elapsed() > Duration::from_secs(2) {
                                log(format!("{e:#}"));
                                last_log = Instant::now();
                            }
                        }
                    }
                }
            }
        });
        Self {
            desktop: name,
            tx,
            thread,
        }
    }

    fn stop(self) {
        let Self { tx, thread, .. } = self;
        let _ = tx.send(Injection::Event(InputEvent::ReleaseAll));
        drop(tx);
        let _ = thread.join();
    }
}

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
    let video_udp = UdpSocket::bind((c.bind.as_str(), c.port)).context("binding UDP video port")?;
    // A short receive timeout lets the same loop service legacy TCP accepts too.
    video_udp.set_read_timeout(Some(Duration::from_millis(2)))?;
    socket2::SockRef::from(&video_udp).set_send_buffer_size(1024 * 1024)?;
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
    let context = SessionContext {
        cfg: cfg.clone(),
        pipeline: pipeline.clone(),
        alive: alive.clone(),
        counters: counters.clone(),
        busy: busy.clone(),
    };
    log(format!(
        "Listening: video UDP {}, video TCP {} (legacy NUT), input TCP {}, files TCP {}, discovery UDP 47992; {}x{} @ {}fps; {:?} {:?} QP {} preset {:?}",
        cfg.port, cfg.port, cfg.input_port,cfg.file_port,cfg.width,cfg.height,cfg.fps,cfg.codec,cfg.rate_control,cfg.qp,cfg.preset
    ));
    let mut active_udp: Option<ActiveUdp> = None;
    let mut datagram = [0u8; 64];
    while alive.load(Ordering::Relaxed) {
        if cfg
            .smoke_seconds
            .is_some_and(|s| start.elapsed().as_secs() >= s)
        {
            break;
        }
        // Clear the tracked session once its thread has released the encoder.
        if active_udp.is_some() && !busy.load(Ordering::SeqCst) {
            active_udp = None;
        }
        match video_udp.recv_from(&mut datagram) {
            Ok((n, peer)) if private_peer(peer.ip()) => {
                if let Some((session, operation)) = udp::parse_request(&datagram[..n]) {
                    match operation {
                        udp::START => {
                            if busy.swap(true, Ordering::SeqCst) {
                                log(format!(
                                    "UDP video request from {peer} ignored: a viewer is already connected"
                                ));
                            } else {
                                match start_udp_session(&context, &video_udp, peer, session) {
                                    Ok(active) => active_udp = Some(active),
                                    Err(e) => {
                                        busy.store(false, Ordering::SeqCst);
                                        log(format!("UDP video session: {e:#}"));
                                    }
                                }
                            }
                        }
                        udp::BEAT => {
                            if let Some(active) = &active_udp {
                                if active.session == session && active.peer == peer {
                                    active.beat.store(
                                        active.started.elapsed().as_millis() as u64,
                                        Ordering::Relaxed,
                                    );
                                }
                            }
                        }
                        udp::STOP => {
                            if let Some(active) = &active_udp {
                                if active.session == session && active.peer == peer {
                                    active.stop.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok((_n, _peer)) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        // A viewer that closes its datagram socket can surface
                        // the pending ICMP error here; it must not stop the server.
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionRefused
                ) => {}
            Err(e) => bail!("UDP video listener: {e}"),
        }
        match video.accept() {
            Ok((stream, peer)) if private_peer(peer.ip()) => {
                if busy.swap(true, Ordering::SeqCst) {
                    continue;
                }
                let context = context.clone();
                std::thread::spawn(move || {
                    log(format!("Video connected (legacy TCP): {peer}"));
                    if let Err(e) = video_session(
                        stream,
                        &context.cfg,
                        &context.pipeline,
                        &context.alive,
                        context.counters.clone(),
                    ) {
                        log(format!("Video session: {e:#}"));
                    }
                    context.busy.store(false, Ordering::SeqCst);
                    log("Video disconnected; ready for the next viewer");
                });
            }
            Ok((_s, _p)) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
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
    let mut capability = [0u8; 1];
    stream.read_exact(&mut capability)?;
    let wants_audio = capability[0] & crate::protocol::CAPABILITY_AUDIO != 0 && !c.no_audio;
    // Fixed 36-byte handshake: magic, desktop rectangle, video geometry, fps, ports.
    let mut hello = MAGIC.to_vec();
    for v in [rect.left, rect.top, rect.width, rect.height] {
        hello.extend_from_slice(&v.to_le_bytes());
    }
    for v in [c.width, c.height, c.fps, c.port, c.input_port, 1] {
        hello.extend_from_slice(&v.to_le_bytes());
    }
    stream.write_all(&hello)?;
    stream.write_all(&[crate::protocol::STATUS_INFO, 0])?;
    write_packet(&mut stream, &serde_json::to_vec(info)?)?;
    // Input is injected from a thread attached to the desktop that currently
    // receives input. A fresh thread is created for each desktop visit, before
    // that thread makes any other User32 call.
    let rejected = Arc::new(AtomicU64::new(0));
    let mut injector: Option<DesktopInjector> = None;
    let mut injector_retry = Instant::now();
    let mut desktop_rect = rect;
    // Cursor/status and audio share the server-to-viewer direction of this socket.
    let status = Arc::new(Mutex::new(stream.try_clone()?));
    let connected = Arc::new(AtomicBool::new(true));
    let status_connected = connected.clone();
    let status_writer = status.clone();
    let status_thread = std::thread::spawn(move || {
        let mut last_handle = 0;
        let mut last_stats = Instant::now();
        while status_connected.load(Ordering::Relaxed) {
            // Cursor state belongs to the desktop receiving input.
            let _ = crate::desktop::follow_input_desktop();
            let (id, handle) = current_cursor();
            if id == 255 && handle != last_handle {
                if let Ok(shape) = crate::cursor::snapshot(handle) {
                    let mut packet = vec![];
                    shape.encode(&mut packet);
                    let Ok(mut guard) = status_writer.lock() else {
                        break;
                    };
                    if guard
                        .write_all(&[crate::protocol::STATUS_CURSOR_SHAPE, 0])
                        .is_err()
                        || write_packet(&mut *guard, &packet).is_err()
                    {
                        break;
                    }
                    last_handle = handle;
                }
            } else if id != 255 {
                last_handle = 0;
            }
            // Send a heartbeat even when the cursor shape has not changed.
            let Ok(mut guard) = status_writer.lock() else {
                break;
            };
            if guard
                .write_all(&[crate::protocol::STATUS_CURSOR, id])
                .is_err()
            {
                break;
            }
            if last_stats.elapsed() >= Duration::from_secs(1) {
                let packet = serde_json::to_vec(&counters.snapshot()).unwrap_or_default();
                if guard
                    .write_all(&[crate::protocol::STATUS_STATS, 0])
                    .is_err()
                    || write_packet(&mut *guard, &packet).is_err()
                {
                    break;
                }
                last_stats = Instant::now();
            }
            drop(guard);
            std::thread::sleep(Duration::from_millis(33));
        }
        // Unblock the input read as soon as status delivery fails.
        if let Ok(guard) = status_writer.lock() {
            let _ = guard.shutdown(std::net::Shutdown::Both);
        }
    });
    let audio_stop = Arc::new(AtomicBool::new(false));
    let audio_thread = if wants_audio {
        let writer = status.clone();
        let stop = audio_stop.clone();
        Some(std::thread::spawn(move || {
            let mut announced = None;
            let result = crate::audio::capture(stop, |format, pcm| {
                send_audio(&writer, &mut announced, format, pcm)
            });
            if let Err(e) = result {
                log(format!("Audio capture: {e:#}"));
            }
        }))
    } else {
        None
    };
    let result = (|| -> Result<()> {
        let mut buf = [0; INPUT_SIZE];
        let (mut keyboard, mut mouse, mut dropped) = (0u64, 0u64, 0u64);
        let mut refresh = Instant::now();
        let output = unsafe {
            CreateDXGIFactory1::<IDXGIFactory1>()?
                .EnumAdapters1(0)?
                .EnumOutputs(c.monitor)?
        };
        while alive.load(Ordering::Relaxed) {
            if let Err(e) = stream.read_exact(&mut buf) {
                log(format!(
                    "Input summary: {keyboard} keyboard events, {mouse} mouse events, {} rejected injections, {dropped} dropped events",
                    rejected.load(Ordering::Relaxed)
                ));
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
                        desktop_rect = DesktopRect {
                            left: r.left,
                            top: r.top,
                            width: r.right - r.left,
                            height: r.bottom - r.top,
                        };
                        if let Some(target) = &injector {
                            let _ = target.tx.try_send(Injection::Rect(desktop_rect));
                        }
                    }
                }
                refresh = Instant::now();
            }
            let input_desktop = match crate::desktop::input_name() {
                Ok(name) => name,
                Err(_) => injector
                    .as_ref()
                    .map(|current| current.desktop.clone())
                    .unwrap_or_else(|| "Default".to_string()),
            };
            let restart = match &injector {
                Some(current) => current.desktop != input_desktop,
                None => Instant::now() >= injector_retry,
            };
            if restart {
                if let Some(previous) = injector.take() {
                    previous.stop();
                }
                injector_retry = Instant::now() + Duration::from_secs(1);
                log(format!(
                    "Input injector: attaching a thread to the {input_desktop} desktop"
                ));
                injector = Some(DesktopInjector::start(
                    &input_desktop,
                    desktop_rect,
                    rejected.clone(),
                ));
            }
            match injector
                .as_mut()
                .map(|target| target.tx.try_send(Injection::Event(event)))
            {
                Some(Ok(())) => {}
                Some(Err(crossbeam_channel::TrySendError::Full(_))) => dropped += 1,
                Some(Err(crossbeam_channel::TrySendError::Disconnected(_))) => {
                    injector = None;
                    dropped += 1;
                    injector_retry = Instant::now() + Duration::from_secs(1);
                }
                None => dropped += 1,
            }
        }
        Ok(())
    })();
    connected.store(false, Ordering::Relaxed);
    audio_stop.store(true, Ordering::Relaxed);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    if let Some(injector) = injector.take() {
        injector.stop();
    }
    if let Some(audio_thread) = audio_thread {
        let _ = audio_thread.join();
    }
    let _ = status_thread.join();
    result
}

/// Write one audio format packet once, then PCM chunks, on the shared status
/// stream. Returns false when the viewer is gone.
fn send_audio(
    writer: &Arc<Mutex<TcpStream>>,
    announced: &mut Option<crate::audio::Format>,
    format: crate::audio::Format,
    pcm: &[i16],
) -> bool {
    let Ok(mut stream) = writer.lock() else {
        return false;
    };
    if announced.as_ref() != Some(&format) {
        let packet = crate::audio::Format::encode(format);
        if stream
            .write_all(&[crate::protocol::STATUS_AUDIO_FORMAT, 0])
            .is_err()
            || write_packet(&mut *stream, &packet).is_err()
        {
            return false;
        }
        *announced = Some(format);
    }
    if pcm.is_empty() {
        return true;
    }
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for sample in pcm {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    stream
        .write_all(&[crate::protocol::STATUS_AUDIO_DATA, 0])
        .is_ok()
        && write_packet(&mut *stream, &bytes).is_ok()
}

struct ActiveUdp {
    session: u64,
    peer: SocketAddr,
    beat: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    started: Instant,
}

/// Everything a video session needs from the listener's scope.
#[derive(Clone)]
struct SessionContext {
    cfg: Arc<ServerConfig>,
    pipeline: media::Pipeline,
    alive: Arc<AtomicBool>,
    counters: Arc<StreamCounters>,
    busy: Arc<AtomicBool>,
}

/// How long a UDP viewer may stay silent before its encoder is stopped.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(2);

fn start_udp_session(
    context: &SessionContext,
    socket: &UdpSocket,
    peer: SocketAddr,
    session: u64,
) -> Result<ActiveUdp> {
    let socket = socket.try_clone()?;
    let beat = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let active = ActiveUdp {
        session,
        peer,
        beat: beat.clone(),
        stop: stop.clone(),
        started,
    };
    let context = context.clone();
    log(format!(
        "UDP video session from {peer}; MPEG-TS groups with XOR parity"
    ));
    std::thread::spawn(move || {
        let result = udp::Sender::new(socket, peer, session).and_then(|sender| {
            run_video(
                Transport::Udp {
                    sender,
                    beat,
                    stop,
                    started,
                },
                &context.cfg,
                &context.pipeline,
                &context.alive,
                context.counters.clone(),
            )
        });
        if let Err(e) = result {
            log(format!("UDP video session: {e:#}"));
        }
        context.busy.store(false, Ordering::SeqCst);
        log("Video disconnected; ready for the next viewer");
    });
    Ok(active)
}

enum Transport {
    /// Retained for `--legacy-buffering` comparisons and old clients.
    Tcp(TcpStream),
    Udp {
        sender: udp::Sender,
        beat: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
        started: Instant,
    },
}

impl Transport {
    fn container(&self) -> Result<(&'static str, &'static [&'static str])> {
        Ok(match self {
            Self::Tcp(_) => ("nut", &["-f", "nut", "-write_index", "0"]),
            // Ask the TS muxer not to hold packets for interleaving.
            Self::Udp { .. } => (
                "mpegts",
                &["-muxdelay", "0", "-muxpreload", "0", "-f", "mpegts"],
            ),
        })
    }

    fn feed(&mut self, data: &[u8], counters: &StreamCounters) -> Result<()> {
        let sent_at = Instant::now();
        match self {
            Self::Tcp(stream) => stream.write_all(data)?,
            Self::Udp { sender, .. } => sender.write(data)?,
        }
        counters
            .send_wait_us
            .fetch_add(sent_at.elapsed().as_micros() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn watch(&self) -> Result<Watch> {
        Ok(match self {
            Self::Tcp(stream) => {
                let stream = stream.try_clone()?;
                stream.set_read_timeout(Some(Duration::from_millis(100)))?;
                Watch::Peer(stream)
            }
            Self::Udp {
                beat,
                stop,
                started,
                ..
            } => Watch::Heartbeat {
                beat: beat.clone(),
                stop: stop.clone(),
                started: *started,
            },
        })
    }

    fn shutdown(&mut self) {
        if let Self::Tcp(stream) = self {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn groups(&self) -> Option<u64> {
        match self {
            Self::Udp { sender, .. } => Some(sender.groups()),
            Self::Tcp(_) => None,
        }
    }
}

enum Watch {
    Peer(TcpStream),
    Heartbeat {
        beat: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
        started: Instant,
    },
}

impl Watch {
    fn failure(&mut self) -> Option<&'static str> {
        match self {
            Self::Peer(stream) => {
                let mut byte = [0];
                match stream.read(&mut byte) {
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
                }
            }
            Self::Heartbeat {
                beat,
                stop,
                started,
            } => {
                if stop.load(Ordering::Relaxed) {
                    return Some("viewer stopped the stream");
                }
                let idle = (started.elapsed().as_millis() as u64)
                    .saturating_sub(beat.load(Ordering::Relaxed));
                (idle > HEARTBEAT_TIMEOUT.as_millis() as u64).then_some("viewer heartbeat lost")
            }
        }
    }
}

fn video_session(
    stream: TcpStream,
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
    run_video(Transport::Tcp(stream), c, p, alive, counters)
}

fn run_video(
    mut transport: Transport,
    c: &ServerConfig,
    p: &media::Pipeline,
    alive: &AtomicBool,
    counters: Arc<StreamCounters>,
) -> Result<()> {
    let mut cmd = media::ffmpeg_command(c, p)?;
    counters.reset();
    cmd.args(transport.container()?.1)
        .args([
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
    let mut watch = transport.watch()?;
    let watching = Arc::new(AtomicBool::new(true));
    let watch_active = watching.clone();
    let last_output = Arc::new(AtomicU64::new(0));
    let observed_output = last_output.clone();
    let started = Instant::now();
    let watchdog = std::thread::spawn(move || {
        while watch_active.load(Ordering::Relaxed) {
            let failure = watch.failure();
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
            std::thread::sleep(Duration::from_millis(100));
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
            transport.feed(&buf[..n], &counters)?;
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
    transport.shutdown();
    let _ = watchdog.join();
    match transport.groups() {
        Some(groups) => log(format!(
            "Sent {bytes} bytes in {groups} UDP groups in {:?}",
            start.elapsed()
        )),
        None => log(format!("Sent {bytes} bytes in {:?}", start.elapsed())),
    }
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
