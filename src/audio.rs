//! Desktop audio: WASAPI loopback capture on the server and WASAPI rendering on
//! the viewer. Audio is 16-bit stereo PCM carried on the existing control TCP
//! connection, so no extra ports or codecs are involved.
//!
//! Loopback captures whatever the server's default output device is playing, so
//! no virtual audio driver is required. The capture endpoint's sample rate is
//! preserved and the viewer's audio engine converts it if its own device runs at
//! a different rate.
use anyhow::{ensure, Context, Result};
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use std::{
    collections::VecDeque,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::RPC_E_CHANGED_MODE,
    Media::{
        Audio::*,
        KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE},
        Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
    },
    System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED,
    },
};

/// PCM format announced to the viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
}

impl Format {
    pub const BYTES: usize = 8;

    pub fn encode(self) -> [u8; Self::BYTES] {
        let mut out = [0; Self::BYTES];
        out[..4].copy_from_slice(&self.rate.to_le_bytes());
        out[4..6].copy_from_slice(&self.channels.to_le_bytes());
        out[6..8].copy_from_slice(&16u16.to_le_bytes());
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        ensure!(data.len() == Self::BYTES, "invalid audio format packet");
        let rate = u32::from_le_bytes(data[..4].try_into()?);
        let channels = u16::from_le_bytes(data[4..6].try_into()?);
        let bits = u16::from_le_bytes(data[6..8].try_into()?);
        ensure!(
            (8_000..=192_000).contains(&rate) && (1..=8).contains(&channels) && bits == 16,
            "unsupported audio format"
        );
        Ok(Self { rate, channels })
    }
}

/// COM is initialized per audio thread and released on exit.
struct Apartment {
    uninitialize: bool,
}

impl Apartment {
    fn init() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            // The thread already joined an apartment; COM is usable.
            return Ok(Self {
                uninitialize: false,
            });
        }
        hr.ok().context("initializing COM for audio")?;
        Ok(Self { uninitialize: true })
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.uninitialize {
            unsafe { CoUninitialize() };
        }
    }
}

/// The endpoint's native mix format, with enough detail to convert to PCM16.
#[derive(Clone, Copy)]
struct MixFormat {
    wfx: WAVEFORMATEX,
    float: bool,
    valid_bits: u16,
}

impl MixFormat {
    /// # Safety
    /// `format` must point to a valid WAVEFORMATEX from `IAudioClient::GetMixFormat`.
    unsafe fn parse(format: *const WAVEFORMATEX) -> Result<Self> {
        ensure!(!format.is_null(), "audio device returned no mix format");
        // WAVEFORMATEX is packed; copy fields by value instead of borrowing them.
        let wfx = *format;
        let (channels, rate, block_align, bits, tag, cb_size) = (
            wfx.nChannels,
            wfx.nSamplesPerSec,
            wfx.nBlockAlign,
            wfx.wBitsPerSample,
            wfx.wFormatTag as u32,
            wfx.cbSize,
        );
        ensure!(
            channels > 0
                && (8_000..=192_000).contains(&rate)
                && block_align > 0
                && matches!(bits, 16 | 24 | 32),
            "unsupported audio mix format"
        );
        let (float, valid_bits) = if tag == WAVE_FORMAT_EXTENSIBLE {
            ensure!(cb_size >= 22, "truncated extensible audio format");
            let ext = *format.cast::<WAVEFORMATEXTENSIBLE>();
            let subformat = ext.SubFormat;
            let samples = ext.Samples;
            let valid_bits = samples.wValidBitsPerSample;
            let float = subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
            ensure!(
                float || subformat == KSDATAFORMAT_SUBTYPE_PCM,
                "unsupported audio sample encoding"
            );
            (float, valid_bits)
        } else {
            (tag == WAVE_FORMAT_IEEE_FLOAT, bits)
        };
        Ok(Self {
            wfx,
            float,
            valid_bits,
        })
    }

    #[cfg(test)]
    fn parts(rate: u32, channels: u16, bits: u16, float: bool) -> Self {
        Self {
            wfx: WAVEFORMATEX {
                wFormatTag: if float {
                    WAVE_FORMAT_IEEE_FLOAT as u16
                } else {
                    WAVE_FORMAT_PCM as u16
                },
                nChannels: channels,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * channels as u32 * (bits / 8) as u32,
                nBlockAlign: channels * bits / 8,
                wBitsPerSample: bits,
                cbSize: 0,
            },
            float,
            valid_bits: bits,
        }
    }

    fn block_align(&self) -> usize {
        self.wfx.nBlockAlign as usize
    }

    fn channels(&self) -> usize {
        self.wfx.nChannels as usize
    }

    fn bytes_per_sample(&self) -> usize {
        (self.wfx.wBitsPerSample / 8) as usize
    }

    fn bits(&self) -> u16 {
        self.wfx.wBitsPerSample
    }

    /// Convert interleaved device samples to stereo PCM16. Multi-channel
    /// endpoints keep the front pair; mono is duplicated.
    fn to_stereo_i16(self, data: &[u8]) -> Vec<i16> {
        let (block, channels, sample) =
            (self.block_align(), self.channels(), self.bytes_per_sample());
        if block == 0 || sample == 0 || data.len() < block {
            return Vec::new();
        }
        let frames = data.len() / block;
        let mut out = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let base = frame * block;
            let left = self.sample(data, base);
            let right = if channels > 1 {
                self.sample(data, base + sample)
            } else {
                left
            };
            out.push(left);
            out.push(right);
        }
        out
    }

    fn sample(&self, data: &[u8], offset: usize) -> i16 {
        match (self.float, self.wfx.wBitsPerSample) {
            (true, 32) => {
                let value = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                (value.clamp(-1.0, 1.0) * 32_767.0).round() as i16
            }
            (false, 16) => i16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()),
            (false, 24) => {
                let raw = (data[offset] as i32)
                    | ((data[offset + 1] as i32) << 8)
                    | ((data[offset + 2] as i32) << 16);
                ((raw << 8) >> 24 << 8) as i16
            }
            (false, 32) => {
                (i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) >> 16) as i16
            }
            _ => 0,
        }
    }
}

/// Capture the default output device until `stop` is set. The sink receives the
/// format first (with an empty slice), then PCM16 stereo chunks; returning false
/// ends the capture, as does a write failure or device change.
pub fn capture(stop: Arc<AtomicBool>, mut sink: impl FnMut(Format, &[i16]) -> bool) -> Result<()> {
    let _com = Apartment::init()?;
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("opening the audio device enumerator")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("opening the default output device")?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("activating the default output device")?;
        let raw = client
            .GetMixFormat()
            .context("reading the output mix format")?;
        let mix = match MixFormat::parse(raw) {
            Ok(mix) => mix,
            Err(e) => {
                CoTaskMemFree(Some(raw as *const c_void));
                return Err(e);
            }
        };
        let format = Format {
            rate: mix.wfx.nSamplesPerSec,
            channels: 2,
        };
        let initialized = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            1_000_000,
            0,
            raw,
            None,
        );
        CoTaskMemFree(Some(raw as *const c_void));
        initialized.context("starting loopback capture")?;
        let capture: IAudioCaptureClient = client.GetService().context("capture service")?;
        client.Start().context("starting audio capture")?;
        crate::util::log(format!(
            "Audio capture: {} Hz, {} ch loopback; device mix {} ch, {} bit, {} valid, float={}",
            format.rate,
            format.channels,
            mix.channels(),
            mix.bits(),
            mix.valid_bits,
            mix.float
        ));
        if !sink(format, &[]) {
            let _ = client.Stop();
            return Ok(());
        }
        while !stop.load(Ordering::Relaxed) {
            let mut available = capture.GetNextPacketSize()?;
            if available == 0 {
                std::thread::sleep(Duration::from_millis(4));
                continue;
            }
            while available > 0 && !stop.load(Ordering::Relaxed) {
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                let mut keep_going = true;
                if !silent && frames > 0 && !data.is_null() {
                    let bytes =
                        std::slice::from_raw_parts(data, frames as usize * mix.block_align());
                    let pcm = mix.to_stereo_i16(bytes);
                    if !pcm.is_empty() {
                        keep_going = sink(format, &pcm);
                    }
                }
                // The capture buffer must be released even when its data is unused.
                capture.ReleaseBuffer(frames)?;
                if !keep_going {
                    let _ = client.Stop();
                    return Ok(());
                }
                available = capture.GetNextPacketSize()?;
            }
        }
        let _ = client.Stop();
    }
    crate::util::log("Audio capture stopped");
    Ok(())
}

#[derive(Default)]
struct Stats {
    received: AtomicU64,
    dropped: AtomicU64,
    played: AtomicU64,
    underruns: AtomicU64,
    peak: AtomicU32,
    active: AtomicBool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub received: u64,
    pub dropped: u64,
    pub played: u64,
    pub underruns: u64,
    pub peak: f32,
    pub active: bool,
}

enum Command {
    Format(Format),
    Data(Vec<u8>),
}

/// Viewer-side audio output. The network thread pushes chunks; a dedicated
/// thread owns the WASAPI render client and keeps a small playout buffer.
pub struct AudioPlayer {
    tx: Sender<Command>,
    stats: Arc<Stats>,
}

impl AudioPlayer {
    pub fn new(buffer_ms: u16) -> Self {
        let (tx, rx) = bounded(8);
        let stats = Arc::new(Stats::default());
        let worker = stats.clone();
        std::thread::spawn(move || render_thread(rx, worker, buffer_ms));
        Self { tx, stats }
    }

    pub fn format(&self, format: Format) {
        let _ = self.tx.try_send(Command::Format(format));
    }

    pub fn samples(&self, data: &[u8]) {
        if data.is_empty() || !data.len().is_multiple_of(2) {
            return;
        }
        self.stats
            .received
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        if self.tx.try_send(Command::Data(data.to_vec())).is_err() {
            self.stats
                .dropped
                .fetch_add(data.len() as u64, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            received: self.stats.received.load(Ordering::Relaxed),
            dropped: self.stats.dropped.load(Ordering::Relaxed),
            played: self.stats.played.load(Ordering::Relaxed),
            underruns: self.stats.underruns.load(Ordering::Relaxed),
            peak: f32::from_bits(self.stats.peak.load(Ordering::Relaxed)),
            active: self.stats.active.load(Ordering::Relaxed),
        }
    }
}

struct Render {
    client: IAudioClient,
    render: IAudioRenderClient,
    frames: u32,
    rate: u32,
    frame_bytes: usize,
    preroll_bytes: usize,
}

impl Render {
    fn new(format: Format, buffer_ms: u16) -> Result<Self> {
        ensure!(format.channels == 2, "audio output expects stereo");
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .context("opening the audio device enumerator")?;
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .context("opening the default output device")?;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .context("activating the default output device")?;
        let frame_bytes = 2 * 2;
        let wave = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: format.channels,
            nSamplesPerSec: format.rate,
            nAvgBytesPerSec: format.rate * frame_bytes as u32,
            nBlockAlign: frame_bytes as u16,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        // Let the audio engine resample to the endpoint's own mix format.
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, &wave, None) }
            .context("opening the default audio output")?;
        let frames = unsafe { client.GetBufferSize() }.context("audio output buffer")?;
        ensure!(frames > 0, "empty audio output buffer");
        let render: IAudioRenderClient =
            unsafe { client.GetService() }.context("audio render service")?;
        let preroll_bytes =
            (format.rate as usize * frame_bytes * buffer_ms as usize / 1000).max(frame_bytes);
        Ok(Self {
            client,
            render,
            frames,
            rate: format.rate,
            frame_bytes,
            preroll_bytes,
        })
    }

    fn pump(
        &mut self,
        ring: &mut VecDeque<u8>,
        playing: &mut bool,
        silent_since: &mut Option<Instant>,
        stats: &Stats,
    ) -> Result<()> {
        let padding = unsafe { self.client.GetCurrentPadding() }?;
        let available = self.frames.saturating_sub(padding);
        if available == 0 {
            return Ok(());
        }
        if !*playing {
            if ring.len() < self.preroll_bytes {
                return Ok(());
            }
            unsafe { self.client.Start() }.context("starting audio playback")?;
            *playing = true;
            *silent_since = None;
            stats.active.store(true, Ordering::Relaxed);
        }
        let ring_frames = (ring.len() / self.frame_bytes) as u32;
        let frames = available.min(ring_frames);
        if frames > 0 {
            let data = unsafe { self.render.GetBuffer(frames) }?;
            let bytes = frames as usize * self.frame_bytes;
            ring.make_contiguous();
            let front = &ring.as_slices().0[..bytes];
            let level = peak(front);
            unsafe { std::ptr::copy_nonoverlapping(front.as_ptr(), data, bytes) };
            ring.drain(..bytes);
            unsafe { self.render.ReleaseBuffer(frames, 0) }?;
            *silent_since = None;
            stats.played.fetch_add(bytes as u64, Ordering::Relaxed);
            stats.peak.store(level, Ordering::Relaxed);
        } else {
            // Keep the endpoint running briefly through a gap, then stop so the
            // next audio does not queue behind stale silence.
            let frames = available.min((self.rate / 100).max(1));
            let data = unsafe { self.render.GetBuffer(frames) }?;
            unsafe { std::ptr::write_bytes(data, 0, frames as usize * self.frame_bytes) };
            unsafe { self.render.ReleaseBuffer(frames, 0) }?;
            stats.underruns.fetch_add(1, Ordering::Relaxed);
            let since = silent_since.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_millis(250) {
                self.stop();
                *playing = false;
                *silent_since = None;
                stats.active.store(false, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn stop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = self.client.Reset();
        }
    }
}

fn peak(data: &[u8]) -> u32 {
    let mut max = 0i16;
    for pair in data.as_chunks::<2>().0 {
        let sample = i16::from_le_bytes([pair[0], pair[1]]);
        max = max.max(sample.saturating_abs());
    }
    (max as f32 / 32_768.0).to_bits()
}

fn render_thread(rx: Receiver<Command>, stats: Arc<Stats>, buffer_ms: u16) {
    let _com = match Apartment::init() {
        Ok(apartment) => apartment,
        Err(e) => {
            crate::util::log(format!("Audio playback unavailable: {e:#}"));
            return;
        }
    };
    let mut render: Option<Render> = None;
    let mut ring: VecDeque<u8> = VecDeque::new();
    let mut playing = false;
    let mut silent_since: Option<Instant> = None;
    let mut max_bytes = 0usize;
    loop {
        match rx.recv_timeout(Duration::from_millis(4)) {
            Ok(Command::Format(format)) => {
                render = None;
                playing = false;
                silent_since = None;
                ring.clear();
                stats.peak.store(0, Ordering::Relaxed);
                stats.active.store(false, Ordering::Relaxed);
                match Render::new(format, buffer_ms) {
                    Ok(output) => {
                        crate::util::log(format!(
                            "Audio renderer: {} Hz, {} ch, {} ms playout buffer",
                            format.rate, format.channels, buffer_ms
                        ));
                        max_bytes =
                            format.rate as usize * 2 * 2 * (buffer_ms as usize + 250) / 1000;
                        render = Some(output);
                    }
                    Err(e) => crate::util::log(format!("Audio renderer unavailable: {e:#}")),
                }
            }
            Ok(Command::Data(bytes)) => {
                if render.is_none() {
                    // No output device: never accumulate audio in memory.
                    stats
                        .dropped
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    continue;
                }
                ring.extend(&bytes);
                if max_bytes > 0 && ring.len() > max_bytes {
                    let excess = ring.len() - max_bytes;
                    ring.drain(..excess);
                    stats.dropped.fetch_add(excess as u64, Ordering::Relaxed);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let Some(output) = render.as_mut() else {
            continue;
        };
        if let Err(e) = output.pump(&mut ring, &mut playing, &mut silent_since, &stats) {
            crate::util::log(format!("Audio playback stopped: {e:#}"));
            render = None;
            playing = false;
            ring.clear();
            stats.active.store(false, Ordering::Relaxed);
        }
    }
    if let Some(output) = render.as_mut() {
        output.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floats(values: &[(f32, f32)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (left, right) in values {
            bytes.extend_from_slice(&left.to_le_bytes());
            bytes.extend_from_slice(&right.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn converts_float_and_integer_samples() {
        let float = MixFormat::parts(48_000, 2, 32, true);
        assert_eq!(
            float.to_stereo_i16(&floats(&[(1.0, -1.0), (0.5, 0.0)])),
            vec![32_767, -32_767, 16_384, 0]
        );
        let pcm = MixFormat::parts(44_100, 1, 16, false);
        let bytes: Vec<u8> = [1_000i16, -1_000]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        assert_eq!(
            pcm.to_stereo_i16(&bytes),
            vec![1_000, 1_000, -1_000, -1_000]
        );
        let packed = MixFormat::parts(48_000, 1, 24, false);
        let bytes: Vec<u8> = [0x40_00_00i32, -0x40_00_00i32]
            .iter()
            .flat_map(|v| [*v as u8, (v >> 8) as u8, (v >> 16) as u8])
            .collect();
        assert_eq!(
            packed.to_stereo_i16(&bytes),
            vec![16_384, 16_384, -16_384, -16_384]
        );
    }

    #[test]
    fn audio_format_roundtrips_and_rejects_garbage() {
        let format = Format {
            rate: 48_000,
            channels: 2,
        };
        assert_eq!(Format::decode(&format.encode()).unwrap(), format);
        assert!(Format::decode(&[0; 7]).is_err());
        let mut wrong = format.encode();
        wrong[6..8].copy_from_slice(&24u16.to_le_bytes());
        assert!(Format::decode(&wrong).is_err());
    }

    /// Manual check: render a short tone, capture it from the same default
    /// device, and require a non-silent loopback chunk. Plays ~0.2 s of 440 Hz.
    #[test]
    #[ignore = "needs an active default audio output device; plays a short tone"]
    fn loopback_captures_rendered_tone() {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_stop = stop.clone();
        let capture_thread = std::thread::spawn(move || {
            capture(worker_stop, move |_format, pcm| {
                if !pcm.is_empty() {
                    let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
                    let _ = tx.send(peak);
                }
                true
            })
        });
        std::thread::sleep(Duration::from_millis(300));
        let player = AudioPlayer::new(50);
        player.format(Format {
            rate: 48_000,
            channels: 2,
        });
        let frames = 48_000 / 5;
        let mut bytes = Vec::with_capacity(frames * 4);
        for frame in 0..frames {
            let value =
                ((frame as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 3_000.0) as i16;
            bytes.extend_from_slice(&value.to_le_bytes());
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        player.samples(&bytes);
        // Wait for the renderer, then look for the tone rather than ambient noise.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut rendered = false;
        let mut peak = 0u16;
        while Instant::now() < deadline {
            if player.snapshot().played > 0 {
                rendered = true;
            }
            if let Ok(value) = rx.recv_timeout(Duration::from_millis(50)) {
                peak = peak.max(value);
            }
            if rendered && peak > 300 {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        capture_thread.join().unwrap().unwrap();
        let snapshot = player.snapshot();
        eprintln!(
            "audio probe: captured_peak={peak} played={} underruns={} dropped={} active={}",
            snapshot.played, snapshot.underruns, snapshot.dropped, snapshot.active
        );
        assert!(snapshot.played > 0, "the tone was never rendered");
        assert!(
            peak > 100,
            "loopback did not capture the rendered tone (peak {peak})"
        );
    }

    #[test]
    fn peak_reports_full_scale() {
        let data = [0u8, 0, 0xff, 0x7f, 0, 0];
        assert!((f32::from_bits(peak(&data)) - 0.99997).abs() < 0.001);
    }
}
