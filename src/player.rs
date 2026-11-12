use crate::{
    latency,
    media::{self, ManagedChild, MpvPlayer},
    render::Renderer,
    telemetry::ViewerStats,
    util::log,
};
use anyhow::{ensure, Context, Result};
use std::{
    cell::{Cell, RefCell},
    io::Read,
    net::ToSocketAddrs,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use windows::Win32::Foundation::HWND;

/// Keep the original player only for controlled diagnostic comparisons.
pub enum Player {
    Native(Box<NativePlayer>),
    Legacy(MpvPlayer),
}
impl Player {
    pub fn new(hwnd: isize, legacy: bool, video_drop_every: Option<u32>) -> Result<Self> {
        if legacy {
            Ok(Self::Legacy(MpvPlayer::new(hwnd, true)?))
        } else {
            Ok(Self::Native(Box::new(NativePlayer::new(
                HWND(hwnd as *mut _),
                video_drop_every.unwrap_or(0),
            )?)))
        }
    }
    pub fn configure(&self, w: u32, h: u32, fps: u16, container: &str) -> Result<()> {
        match self {
            Self::Native(p) => {
                p.size.set((w, h));
                p.fps.set(fps);
                *p.container.borrow_mut() = container.into();
                Ok(())
            }
            Self::Legacy(p) => p.option("demuxer-lavf-format", container),
        }
    }
    pub fn load(&self, url: &str) -> Result<()> {
        match self {
            Self::Native(p) => p.load(url),
            Self::Legacy(p) => p.load(url),
        }
    }
    pub fn stop(&self) {
        match self {
            Self::Native(p) => p.stop(),
            Self::Legacy(p) => p.stop(),
        }
    }
    pub fn property(&self, name: &str) -> Option<String> {
        match self {
            Self::Native(p) => p.property(name),
            Self::Legacy(p) => p.property(name),
        }
    }
    pub fn poll(&self) -> Option<(i32, i32)> {
        match self {
            Self::Native(p) => p.poll(),
            Self::Legacy(p) => p.poll(),
        }
    }
    pub fn frame_age(&self, sw: usize, sh: usize) -> Result<(u32, u32)> {
        match self {
            Self::Native(p) => p.frame_age(sw, sh),
            Self::Legacy(p) => p.frame_age(sw, sh),
        }
    }
    pub fn stats(&self) -> Option<ViewerStats> {
        match self {
            Self::Native(p) => p.stats(),
            Self::Legacy(_) => None,
        }
    }
}

struct Frame {
    pixels: Vec<u8>,
    sequence: u64,
}
struct Session {
    child: ManagedChild,
    thread: Option<std::thread::JoinHandle<()>>,
    active: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Frame>>>,
    recycle: crossbeam_channel::Sender<Vec<u8>>,
    errors: crossbeam_channel::Receiver<String>,
    received: Arc<AtomicU64>,
    skipped: Arc<AtomicU64>,
    bridge: Option<crate::udp::Bridge>,
    started: Instant,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
        // Stop the datagram bridge before the decoder so its blocking stdin
        // write ends quietly instead of looking like a decoder failure.
        self.bridge.take();
        let _ = self.child.child.kill();
        let _ = self.child.child.wait();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub struct NativePlayer {
    renderer: RefCell<Renderer>,
    session: RefCell<Option<Session>>,
    last_pixels: RefCell<Option<Vec<u8>>>,
    size: Cell<(u32, u32)>,
    fps: Cell<u16>,
    container: RefCell<String>,
    sequence: Cell<u64>,
    eof: Cell<bool>,
    presented: Cell<u64>,
    render_us: Cell<u64>,
    drop_every: u32,
}
impl NativePlayer {
    fn new(hwnd: HWND, drop_every: u32) -> Result<Self> {
        Ok(Self {
            renderer: RefCell::new(Renderer::new(hwnd)?),
            session: RefCell::new(None),
            last_pixels: RefCell::new(None),
            size: Cell::new((1920, 1080)),
            fps: Cell::new(60),
            container: RefCell::new("mpegts".into()),
            sequence: Cell::new(0),
            eof: Cell::new(false),
            presented: Cell::new(0),
            render_us: Cell::new(0),
            drop_every,
        })
    }
    fn load(&self, url: &str) -> Result<()> {
        self.stop();
        let (w, h) = self.size.get();
        ensure!(
            w > 0 && h > 0 && w <= 7680 && h <= 4320 && w.is_multiple_of(2) && h.is_multiple_of(2),
            "invalid server geometry"
        );
        // Datagram video is reassembled in Rust and written to the decoder's
        // stdin; the TCP path keeps handing the URL to FFmpeg's demuxer.
        let datagram = url
            .strip_prefix("udp://")
            .map(|address| {
                address
                    .to_socket_addrs()?
                    .next()
                    .context("invalid UDP video address")
            })
            .transpose()?;
        let mut cmd = Command::new(media::runtime_file("ffmpeg.exe")?);
        cmd.creation_flags(0x08000000)
            .stdin(if datagram.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::from(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(std::env::current_exe()?.with_file_name("decoder.log"))?,
            ));
        // MPEG-TS needs enough input to read PAT/PMT; NUT was already minimal.
        let (probe_size, analyze) = if datagram.is_some() {
            ("65536", "0")
        } else {
            ("32", "1")
        };
        cmd.args([
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "verbose",
            "-nostats",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-flags",
            "+low_delay",
            "-probesize",
            probe_size,
            "-analyzeduration",
            analyze,
            "-hwaccel",
            "d3d11va",
            "-hwaccel_output_format",
            "d3d11",
            "-f",
            &self.container.borrow(),
            "-i",
            if datagram.is_some() { "pipe:0" } else { url },
            "-map",
            "0:v:0",
            "-an",
            "-vf",
            "hwdownload,format=nv12",
            "-fps_mode",
            "passthrough",
            "-c:v",
            "rawvideo",
            "-pix_fmt",
            "nv12",
            "-threads:v",
            "1",
            "-f",
            "rawvideo",
            "-flush_packets",
            "1",
            "pipe:1",
        ]);
        let mut child = ManagedChild::spawn(&mut cmd)?;
        let bridge = match datagram {
            Some(address) => {
                let stdin = child.child.stdin.take().context("UDP decoder input")?;
                Some(crate::udp::Bridge::start(address, stdin, self.drop_every)?)
            }
            None => None,
        };
        let mut output = child.child.stdout.take().context("decoder stdout")?;
        let active = Arc::new(AtomicBool::new(true));
        let running = active.clone();
        let latest = Arc::new(Mutex::new(None::<Frame>));
        let frames = latest.clone();
        let (recycle, recycled) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (errors_tx, errors) = crossbeam_channel::unbounded();
        let received = Arc::new(AtomicU64::new(0));
        let decoded = received.clone();
        let skipped = Arc::new(AtomicU64::new(0));
        let overwritten = skipped.clone();
        let thread = std::thread::spawn(move || {
            let size = w as usize * h as usize * 3 / 2;
            let mut pixels = vec![0; size];
            let mut sequence = 0;
            while running.load(Ordering::Relaxed) {
                let read_start = Instant::now();
                if let Err(e) = output.read_exact(&mut pixels) {
                    if running.load(Ordering::Relaxed) {
                        let _ = errors_tx.send(e.to_string());
                    }
                    break;
                }
                if read_start.elapsed() > std::time::Duration::from_secs(1) {
                    log(format!(
                        "Decoder frame read took {:?} (frame {})",
                        read_start.elapsed(),
                        sequence + 1
                    ));
                }
                sequence += 1;
                decoded.store(sequence, Ordering::Relaxed);
                let previous = frames.lock().unwrap().replace(Frame { pixels, sequence });
                pixels = if let Some(frame) = previous {
                    overwritten.fetch_add(1, Ordering::Relaxed);
                    frame.pixels
                } else if let Ok(buffer) = recycled.try_recv() {
                    buffer
                } else {
                    vec![0; size]
                };
            }
            running.store(false, Ordering::Relaxed);
        });
        self.eof.set(false);
        self.sequence.set(0);
        self.presented.set(0);
        self.render_us.set(0);
        *self.session.borrow_mut() = Some(Session {
            child,
            thread: Some(thread),
            active,
            latest,
            recycle,
            errors,
            received,
            skipped,
            bridge,
            started: Instant::now(),
        });
        log("Native Direct3D11 viewer: hardware decoder -> NV12 readback -> GPU color conversion/presentation; newest frame only");
        Ok(())
    }
    fn stop(&self) {
        self.session.borrow_mut().take();
        self.last_pixels.borrow_mut().take();
        self.sequence.set(0);
        self.eof.set(false);
    }
    fn poll(&self) -> Option<(i32, i32)> {
        let session = self.session.borrow();
        let session = session.as_ref()?;
        let frame = session.latest.lock().unwrap().take();
        if let Some(frame) = frame {
            let (w, h) = self.size.get();
            let start = Instant::now();
            match self.renderer.borrow_mut().present(&frame.pixels, w, h) {
                Ok(true) => {
                    self.presented.set(self.presented.get() + 1);
                    self.render_us
                        .set(self.render_us.get() + start.elapsed().as_micros() as u64);
                    if self.sequence.get() == 0 {
                        log(format!(
                            "Native frame presented; GPU upload/presentation {:?}",
                            start.elapsed()
                        ));
                    }
                    self.sequence.set(frame.sequence);
                    if let Some(old) = self.last_pixels.borrow_mut().replace(frame.pixels) {
                        let _ = session.recycle.send(old);
                    }
                }
                Ok(false) => {
                    let _ = session.recycle.send(frame.pixels);
                }
                Err(e) => {
                    log(format!("Renderer: {e:#}"));
                    if !self.eof.replace(true) {
                        return Some((7, -1));
                    }
                }
            }
        }
        if !session.active.load(Ordering::Relaxed) && !self.eof.replace(true) {
            if let Ok(e) = session.errors.try_recv() {
                log(format!("Hardware decoder ended: {e}"));
            }
            return Some((7, -1));
        }
        None
    }
    fn property(&self, name: &str) -> Option<String> {
        if self.sequence.get() == 0 {
            return None;
        }
        match name {
            "hwdec-current" => Some("d3d11va".into()),
            "time-pos/full" => Some(format!(
                "{:.6}",
                self.sequence.get() as f64 / self.fps.get() as f64
            )),
            "demuxer-cache-duration" => Some("0".into()),
            "video-sync" => Some("immediate".into()),
            _ => None,
        }
    }
    fn frame_age(&self, sw: usize, sh: usize) -> Result<(u32, u32)> {
        let before = latency::tick();
        let pixels = self.last_pixels.borrow();
        let pixels = pixels.as_ref().context("no displayed frame")?;
        let (w, h) = self.size.get();
        let stamp = latency::read_marker_y(pixels, w as usize, h as usize, sw, sh)?;
        let now = latency::tick();
        let age = now.wrapping_sub(stamp);
        ensure!(age < 60000, "clock not from this computer");
        Ok((age, now.wrapping_sub(before)))
    }
    fn stats(&self) -> Option<ViewerStats> {
        let session = self.session.borrow();
        let s = session.as_ref()?;
        let udp = s
            .bridge
            .as_ref()
            .map(|bridge| bridge.stats())
            .unwrap_or_default();
        Some(ViewerStats {
            decoded: s.received.load(Ordering::Relaxed),
            presented: self.presented.get(),
            skipped: s.skipped.load(Ordering::Relaxed),
            render_us: self.render_us.get(),
            elapsed_ms: s.started.elapsed().as_millis() as u64,
            udp_received: udp.received,
            udp_recovered: udp.recovered,
            udp_missing: udp.lost,
            udp_late: udp.late,
        })
    }
}
