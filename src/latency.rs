//! Opt-in loopback instrumentation. The visible source clock goes through the
//! actual desktop capture, encoder, network and hardware decoder. This measures
//! decoded-frame age, not monitor scanout or input latency. No screenshots saved.
use anyhow::{ensure, Result};
use std::sync::mpsc;
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{LibraryLoader::GetModuleHandleW, SystemInformation::GetTickCount64},
        UI::WindowsAndMessaging::*,
    },
};

pub const CELL: i32 = 8;
pub const BITS: usize = 56;
pub const HEIGHT: i32 = 24;

fn checksum(t: u32) -> u16 {
    t as u16 ^ (t >> 16) as u16 ^ 0xa55a
}
fn bits(t: u32) -> u64 {
    0xd3u64 | ((t as u64) << 8) | ((checksum(t) as u64) << 40)
}
pub fn tick() -> u32 {
    unsafe { GetTickCount64() as u32 }
}

unsafe extern "system" fn clock_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            let _ = InvalidateRect(Some(hwnd), None, false);
            return LRESULT(0);
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            let value = bits(tick());
            for i in 0..BITS {
                let color = if value & (1u64 << i) != 0 {
                    WHITE_BRUSH
                } else {
                    BLACK_BRUSH
                };
                let r = RECT {
                    left: i as i32 * CELL,
                    top: 0,
                    right: (i as i32 + 1) * CELL,
                    bottom: HEIGHT,
                };
                FillRect(dc, &r, HBRUSH(GetStockObject(color).0));
            }
            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        WM_NCHITTEST => return LRESULT(HTTRANSPARENT as isize),
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}
pub struct Marker {
    hwnd: usize,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Marker {
    pub fn start() -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || unsafe {
            let result = (|| -> Result<HWND> {
                let module = GetModuleHandleW(None)?;
                let class = w!("ScreenerLatencyClock");
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(clock_proc),
                    hInstance: HINSTANCE(module.0),
                    lpszClassName: class,
                    ..Default::default()
                };
                ensure!(RegisterClassW(&wc) != 0, "clock window registration");
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    class,
                    w!("Screener latency clock"),
                    WS_POPUP | WS_VISIBLE,
                    0,
                    0,
                    CELL * BITS as i32,
                    HEIGHT,
                    None,
                    None,
                    Some(HINSTANCE(module.0)),
                    None,
                )?;
                ensure!(SetTimer(Some(hwnd), 1, 8, None) != 0, "clock timer");
                Ok(hwnd)
            })();
            match result {
                Ok(hwnd) => {
                    let _ = tx.send(Ok(hwnd.0 as usize));
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        });
        Ok(Self {
            hwnd: rx.recv()??,
            thread: Some(thread),
        })
    }
}
impl Drop for Marker {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn read_marker(
    pixels: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    source_w: usize,
    source_h: usize,
) -> Result<u32> {
    ensure!(
        source_w >= CELL as usize * BITS
            && source_h >= HEIGHT as usize
            && stride >= width * 4
            && pixels.len() >= stride * height,
        "invalid clock image dimensions"
    );
    let y = ((HEIGHT as f64 * 0.5) * height as f64 / source_h as f64) as usize;
    let mut value = 0u64;
    for i in 0..BITS {
        let x = (((i as f64 + 0.5) * CELL as f64) * width as f64 / source_w as f64) as usize;
        ensure!(x < width && y < height, "clock outside frame");
        let p = &pixels[y * stride + x * 4..][..3];
        if p.iter().map(|v| *v as u32).sum::<u32>() > 3 * 128 {
            value |= 1 << i;
        }
    }
    ensure!(
        value as u8 == 0xd3,
        "source clock not visible (primary output must start at 0,0)"
    );
    let t = (value >> 8) as u32;
    ensure!(
        (value >> 40) as u16 == checksum(t),
        "clock changed during capture"
    );
    Ok(t)
}

pub fn read_marker_y(
    pixels: &[u8],
    width: usize,
    height: usize,
    source_w: usize,
    source_h: usize,
) -> Result<u32> {
    ensure!(
        source_w >= CELL as usize * BITS
            && source_h >= HEIGHT as usize
            && pixels.len() >= width * height,
        "invalid clock image dimensions"
    );
    let y = (HEIGHT as f64 * 0.5 * height as f64 / source_h as f64) as usize;
    let mut value = 0u64;
    for i in 0..BITS {
        let x = ((i as f64 + 0.5) * CELL as f64 * width as f64 / source_w as f64) as usize;
        ensure!(x < width && y < height, "clock outside frame");
        if pixels[y * width + x] > 128 {
            value |= 1 << i;
        }
    }
    ensure!(value as u8 == 0xd3, "source clock not visible");
    let t = (value >> 8) as u32;
    ensure!(
        (value >> 40) as u16 == checksum(t),
        "clock changed during capture"
    );
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn barcode_roundtrip_and_corruption() {
        let w = CELL as usize * BITS;
        let h = HEIGHT as usize;
        let stamp = 0x1234abcd;
        let b = bits(stamp);
        let mut p = vec![0; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                if b & (1 << (x / CELL as usize)) != 0 {
                    p[(y * w + x) * 4..][..3].fill(255);
                }
            }
        }
        assert_eq!(read_marker(&p, w, h, w * 4, w, h).unwrap(), stamp);
        let y: Vec<u8> = p
            .as_chunks::<4>()
            .0
            .iter()
            .map(|v| if v[0] > 128 { 235 } else { 16 })
            .collect();
        assert_eq!(read_marker_y(&y, w, h, w, h).unwrap(), stamp);
        p[(h / 2 * w + CELL as usize / 2) * 4..][..3].fill(0);
        assert!(read_marker(&p, w, h, w * 4, w, h).is_err());
    }
}
