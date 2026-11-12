//! Desktop Duplication is paced in this Rust process, where timer precision is
//! explicitly requested. Capture and encoder input have a one-frame handoff.
use crate::{
    telemetry::StreamCounters,
    util::{log, TimerResolution},
};
use anyhow::{ensure, Context, Result};
use std::{
    io::Write,
    process::ChildStdin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows::{
    core::Interface,
    Win32::{
        Foundation::HMODULE,
        Graphics::{Direct3D::*, Direct3D11::*, Dxgi::Common::*, Dxgi::*},
    },
};

struct Desktop {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    width: usize,
    height: usize,
    valid: bool,
}
impl Desktop {
    fn new(output: u32) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let adapter = factory.EnumAdapters1(0)?;
            let output: IDXGIOutput1 = adapter.EnumOutputs(output)?.cast()?;
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.context("capture device")?;
            let context = context.context("capture context")?;
            let dup = if let Ok(output5) = output.cast::<IDXGIOutput5>() {
                output5
                    .DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
                    .or_else(|_| output.DuplicateOutput(&device))?
            } else {
                output.DuplicateOutput(&device)?
            };
            let desc = dup.GetDesc();
            log(format!(
                "Native duplication mode {}x{} {:?}",
                desc.ModeDesc.Width, desc.ModeDesc.Height, desc.ModeDesc.Format
            ));
            // ModeDesc describes the display mode. Validate the acquired texture
            // below rather than assuming it also describes the resource format.
            let texture = D3D11_TEXTURE2D_DESC {
                Width: desc.ModeDesc.Width,
                Height: desc.ModeDesc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            };
            let mut staging = None;
            device.CreateTexture2D(&texture, None, Some(&mut staging))?;
            Ok(Self {
                _device: device,
                context,
                dup,
                staging: staging.context("readback texture")?,
                width: desc.ModeDesc.Width as usize,
                height: desc.ModeDesc.Height as usize,
                valid: false,
            })
        }
    }
    fn update(&mut self, pixels: &mut Arc<Vec<u8>>) -> Result<()> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            match self.dup.AcquireNextFrame(0, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            let result = (|| -> Result<()> {
                if self.valid && info.LastPresentTime == 0 {
                    return Ok(());
                }
                let texture: ID3D11Texture2D = resource.context("desktop texture")?.cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                ensure!(
                    desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM,
                    "capture requires BGRA8; acquired {:?}",
                    desc.Format
                );
                ensure!(
                    desc.Width as usize == self.width && desc.Height as usize == self.height,
                    "desktop resolution changed"
                );
                self.context.CopyResource(&self.staging, &texture);
                let mut map = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
                if Arc::get_mut(pixels).is_none() {
                    *pixels = Arc::new(vec![0; self.width * self.height * 4]);
                }
                let data = Arc::get_mut(pixels).unwrap();
                data.resize(self.width * self.height * 4, 0);
                for y in 0..self.height {
                    std::ptr::copy_nonoverlapping(
                        (map.pData as *const u8).add(y * map.RowPitch as usize),
                        data.as_mut_ptr().add(y * self.width * 4),
                        self.width * 4,
                    );
                }
                self.context.Unmap(&self.staging, 0);
                self.valid = true;
                Ok(())
            })();
            let _ = self.dup.ReleaseFrame();
            result
        }
    }
}

pub struct Feed {
    active: Arc<AtomicBool>,
    capture: Option<std::thread::JoinHandle<()>>,
    writer: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Feed {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
    }
}
impl Feed {
    pub fn start(
        mut stdin: ChildStdin,
        output: u32,
        fps: u16,
        counters: Arc<StreamCounters>,
    ) -> Self {
        let active = Arc::new(AtomicBool::new(true));
        let running = active.clone();
        let writing = active.clone();
        let (tx, rx) = crossbeam_channel::bounded::<Arc<Vec<u8>>>(1);
        let replace = rx.clone();
        let captured = counters.clone();
        let capture = std::thread::spawn(move || {
            let _precision = TimerResolution::new();
            // Duplication must see physical pixels, including under Windows
            // display scaling. FFmpeg's ddagrab establishes the same context.
            unsafe {
                windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
                    windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                );
            }
            let result = (|| -> Result<()> {
                let mut desktop = Desktop::new(output)?;
                log(format!(
                    "Native capture {}x{} @ {fps} FPS; one pending frame",
                    desktop.width, desktop.height
                ));
                let mut pixels = Arc::new(vec![0; desktop.width * desktop.height * 4]);
                let period = Duration::from_secs_f64(1.0 / fps as f64);
                let mut next = Instant::now();
                while running.load(Ordering::Relaxed) {
                    desktop.update(&mut pixels)?;
                    if desktop.valid {
                        captured.captured.fetch_add(1, Ordering::Relaxed);
                        if tx.is_full() {
                            let _ = replace.try_recv();
                        }
                        let _ = tx.try_send(pixels.clone());
                    }
                    next = next_deadline(next, Instant::now(), period);
                    wait_until(next);
                }
                Ok(())
            })();
            if let Err(e) = result {
                log(format!("Native capture ended: {e:#}"));
            }
            running.store(false, Ordering::Relaxed);
        });
        let writer = std::thread::spawn(move || {
            while writing.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(pixels) => {
                        if stdin.write_all(&pixels).is_err() {
                            break;
                        }
                        counters.submitted.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    Err(_) => {}
                }
            }
            writing.store(false, Ordering::Relaxed);
        });
        Self {
            active,
            capture: Some(capture),
            writer: Some(writer),
        }
    }
    pub fn stop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
        if let Some(t) = self.writer.take() {
            let _ = t.join();
        }
        if let Some(t) = self.capture.take() {
            let _ = t.join();
        }
    }
}

// A missed tick is discarded. Never repay timing debt by feeding the encoder
// several old images back-to-back after a stall or an idle-to-motion transition.
fn next_deadline(previous: Instant, now: Instant, period: Duration) -> Instant {
    let next = previous + period;
    if next > now {
        next
    } else {
        now + period
    }
}

fn wait_until(deadline: Instant) {
    const SPIN: Duration = Duration::from_micros(250);
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining > SPIN {
        std::thread::sleep(remaining - SPIN);
    }
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacing_keeps_cadence_and_discards_stall_debt() {
        let t = Instant::now();
        let period = Duration::from_secs_f64(1.0 / 120.0);
        assert_eq!(next_deadline(t, t + period / 2, period), t + period);
        let resumed = t + Duration::from_secs(4);
        let next = next_deadline(t, resumed, period);
        assert_eq!(next, resumed + period);
        assert_eq!(
            next_deadline(next, next + period / 2, period),
            next + period
        );
    }
}
