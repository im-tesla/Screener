use crate::{
    config::ViewerConfig,
    discovery,
    input::{cursor_ids, map_point, map_point_clamped, INJECTION_TAG},
    player::Player,
    protocol::{read_packet, CursorShape, InputEvent, Reader, MAGIC},
    telemetry::{ServerInfo, ServerStats, ViewerStats},
    util::log,
};
use anyhow::{ensure, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use std::{
    cell::{Cell, RefCell},
    io::{Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::*,
            Input::{KeyboardAndMouse::*, *},
            Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP},
            WindowsAndMessaging::*,
        },
    },
};

const WM_FULLSCREEN: u32 = WM_APP + 1;
const VIDEO_TIMER: usize = 1;
static STATE: AtomicPtr<State> = AtomicPtr::new(std::ptr::null_mut());

struct Shared {
    alive: AtomicBool,
    connected: AtomicBool,
    cursor: AtomicU8,
    file_address: Mutex<Option<SocketAddr>>,
}
enum NetworkEvent {
    Connected {
        url: String,
        width: i32,
        height: i32,
        fps: u16,
        source_w: usize,
        source_h: usize,
        container: &'static str,
    },
    Disconnected(String),
    Cursor(CursorShape),
    Info(ServerInfo),
    Stats(ServerStats),
    Transfer(String),
}
struct State {
    hwnd: Cell<HWND>,
    video: Cell<HWND>,
    tx: Sender<InputEvent>,
    shared: Arc<Shared>,
    active: Cell<bool>,
    relative: Cell<bool>,
    view_only: bool,
    fullscreen: Cell<bool>,
    old_rect: Cell<RECT>,
    vw: Cell<i32>,
    vh: Cell<i32>,
    hotkeys: Cell<u8>,
    modifiers: Cell<u8>,
    custom_cursor: RefCell<Option<crate::cursor::NativeCursor>>,
    mouse_buttons: Cell<u8>,
    file_tx: Sender<Vec<PathBuf>>,
    playback: RefCell<Option<Rc<Player>>>,
    playback_events: RefCell<Vec<(i32, i32)>>,
    polling_video: Cell<bool>,
}
impl State {
    fn poll_video(&self) {
        if !self.shared.alive.load(Ordering::Relaxed) || self.polling_video.replace(true) {
            return;
        }
        if let Some(player) = self.playback.borrow().as_ref() {
            while let Some(event) = player.poll() {
                self.playback_events.borrow_mut().push(event);
            }
        }
        self.polling_video.set(false);
    }
    fn send(&self, e: InputEvent) {
        if !self.view_only && self.shared.connected.load(Ordering::Relaxed) {
            let _ = self.tx.send(e);
        }
    }
    fn focused(&self) -> bool {
        unsafe { GetAncestor(GetForegroundWindow(), GA_ROOT) == self.hwnd.get() }
    }
    fn release(&self) {
        self.send(InputEvent::ReleaseAll);
        self.active.set(false);
        self.mouse_buttons.set(0);
        unsafe {
            let _ = ReleaseCapture();
        }
        self.unclip();
    }
    fn unclip(&self) {
        unsafe {
            let _ = ClipCursor(None);
            SetCursor(LoadCursorW(None, IDC_ARROW).ok());
        }
    }
    fn clip(&self) {
        unsafe {
            if self.relative.get() && self.active.get() && self.focused() && !self.view_only {
                let mut r = RECT::default();
                let _ = GetClientRect(self.hwnd.get(), &mut r);
                let mut a = POINT {
                    x: r.left,
                    y: r.top,
                };
                let mut b = POINT {
                    x: r.right,
                    y: r.bottom,
                };
                let _ = ClientToScreen(self.hwnd.get(), &mut a);
                let _ = ClientToScreen(self.hwnd.get(), &mut b);
                let r = RECT {
                    left: a.x,
                    top: a.y,
                    right: b.x,
                    bottom: b.y,
                };
                let _ = ClipCursor(Some(&r));
                SetCursor(None);
            } else {
                self.unclip();
            }
        }
    }
    fn apply_cursor(&self) {
        unsafe {
            let mut point = POINT::default();
            let mut rect = RECT::default();
            if GetCursorPos(&mut point).is_err() {
                return;
            }
            let _ = ScreenToClient(self.hwnd.get(), &mut point);
            let _ = GetClientRect(self.hwnd.get(), &mut rect);
            if point.x < 0 || point.y < 0 || point.x >= rect.right || point.y >= rect.bottom {
                return;
            }
            if self.relative.get() && self.active.get() && self.focused() && !self.view_only {
                SetCursor(None);
                return;
            }
            let id = self.shared.cursor.load(Ordering::Relaxed);
            let ids = cursor_ids();
            let cursor = if id == 255 {
                self.custom_cursor.borrow().as_ref().map(|c| c.0)
            } else if id > 0 {
                LoadCursorW(None, ids[(id as usize - 1).min(ids.len() - 1)]).ok()
            } else {
                None
            };
            SetCursor(cursor.or_else(|| LoadCursorW(None, IDC_ARROW).ok()));
        }
    }
    fn mouse(&self, m: RAWMOUSE) {
        if self.view_only || !self.focused() || !self.shared.connected.load(Ordering::Relaxed) {
            return;
        }
        let b = unsafe { m.Anonymous.Anonymous };
        if !self.active.get() {
            if b.usButtonFlags & 1 != 0 {
                self.active.set(true);
                self.clip();
            } else {
                return;
            }
        }
        if self.relative.get() {
            if m.usFlags.0 & 1 == 0 && (m.lLastX != 0 || m.lLastY != 0) {
                self.send(InputEvent::Relative {
                    x: m.lLastX,
                    y: m.lLastY,
                });
            }
        } else {
            unsafe {
                let mut p = POINT::default();
                let mut r = RECT::default();
                if GetCursorPos(&mut p).is_ok() {
                    let _ = ScreenToClient(self.hwnd.get(), &mut p);
                    let _ = GetClientRect(self.hwnd.get(), &mut r);
                    let mapping = if self.mouse_buttons.get() != 0 {
                        map_point_clamped
                    } else {
                        map_point
                    };
                    if let Some((x, y)) =
                        mapping(p.x, p.y, r.right, r.bottom, self.vw.get(), self.vh.get())
                    {
                        self.send(InputEvent::Absolute { x, y });
                    } else {
                        return;
                    }
                }
            }
        }
        for (button, down, up) in [
            (0, 0x1, 0x2),
            (1, 0x4, 0x8),
            (2, 0x10, 0x20),
            (3, 0x40, 0x80),
            (4, 0x100, 0x200),
        ] {
            if b.usButtonFlags & down != 0 {
                self.mouse_buttons
                    .set(self.mouse_buttons.get() | (1 << button));
                unsafe {
                    SetCapture(self.hwnd.get());
                }
                self.send(InputEvent::Button { button, down: true });
            }
            if b.usButtonFlags & up != 0 {
                self.mouse_buttons
                    .set(self.mouse_buttons.get() & !(1 << button));
                self.send(InputEvent::Button {
                    button,
                    down: false,
                });
            }
        }
        if self.mouse_buttons.get() == 0 {
            unsafe {
                let _ = ReleaseCapture();
            }
        }
        if b.usButtonFlags & 0x400 != 0 {
            self.send(InputEvent::Wheel {
                delta: b.usButtonData as i16 as i32,
                horizontal: false,
            });
        }
        if b.usButtonFlags & 0x800 != 0 {
            self.send(InputEvent::Wheel {
                delta: b.usButtonData as i16 as i32,
                horizontal: true,
            });
        }
    }
    unsafe fn fullscreen(&self) {
        let hwnd = self.hwnd.get();
        if !self.fullscreen.get() {
            let mut old = RECT::default();
            let _ = GetWindowRect(hwnd, &mut old);
            self.old_rect.set(old);
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
                let r = info.rcMonitor;
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_FRAMECHANGED | SWP_NOZORDER,
                );
            }
        } else {
            SetWindowLongPtrW(
                hwnd,
                GWL_STYLE,
                (WS_OVERLAPPEDWINDOW | WS_VISIBLE | WS_CLIPCHILDREN).0 as isize,
            );
            let r = self.old_rect.get();
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_FRAMECHANGED | SWP_NOZORDER,
            );
        }
        self.fullscreen.set(!self.fullscreen.get());
        self.clip();
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ptr = STATE.load(Ordering::Relaxed);
    if code >= 0 && !ptr.is_null() {
        let s = &*ptr;
        let k = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        // Do not re-forward injected events during loopback diagnostics.
        if s.focused() && k.dwExtraInfo != INJECTION_TAG {
            let up = k.flags.0 & LLKHF_UP.0 != 0;
            let modifier = match k.vkCode as u16 {
                0x11 | 0xa2 => 1,
                0xa3 => 2,
                0x12 | 0xa4 => 4,
                0xa5 => 8,
                _ => 0,
            };
            if modifier != 0 {
                s.modifiers.set(if up {
                    s.modifiers.get() & !modifier
                } else {
                    s.modifiers.get() | modifier
                });
            }
            if k.vkCode == VK_F8.0 as u32 {
                if !up && s.hotkeys.get() & 1 == 0 {
                    s.send(InputEvent::ReleaseAll);
                    s.relative.set(!s.relative.get());
                    s.active.set(true);
                    s.clip();
                }
                s.hotkeys.set(if up {
                    s.hotkeys.get() & !1
                } else {
                    s.hotkeys.get() | 1
                });
                return LRESULT(1);
            }
            if k.vkCode == VK_F11.0 as u32 {
                if !up && s.hotkeys.get() & 2 == 0 {
                    let _ = PostMessageW(Some(s.hwnd.get()), WM_FULLSCREEN, WPARAM(0), LPARAM(0));
                }
                s.hotkeys.set(if up {
                    s.hotkeys.get() & !2
                } else {
                    s.hotkeys.get() | 2
                });
                return LRESULT(1);
            }
            if k.vkCode == VK_F12.0 as u32
                && s.modifiers.get() & 3 != 0
                && s.modifiers.get() & 12 != 0
            {
                if !up {
                    s.release();
                }
                return LRESULT(1);
            }
            if s.active.get() && s.shared.connected.load(Ordering::Relaxed) {
                let extended = if k.flags.0 & LLKHF_EXTENDED.0 != 0 {
                    2
                } else {
                    0
                };
                s.send(InputEvent::Key {
                    scan: k.scanCode as u16,
                    vk: k.vkCode as u16,
                    flags: u8::from(up)
                        | extended
                        | if k.vkCode == VK_PAUSE.0 as u32 { 4 } else { 0 },
                });
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = STATE.load(Ordering::Relaxed);
    if !ptr.is_null() {
        let s = &*ptr;
        match msg {
            // Windows runs a nested message loop while dragging/resizing a
            // window or using its system menu. Keep presenting there too.
            WM_TIMER if wp.0 == VIDEO_TIMER => {
                s.poll_video();
                return LRESULT(0);
            }
            WM_INPUT => {
                let mut raw = RAWINPUT::default();
                let mut size = std::mem::size_of::<RAWINPUT>() as u32;
                let n = GetRawInputData(
                    HRAWINPUT(lp.0 as *mut _),
                    RID_INPUT,
                    Some(&mut raw as *mut _ as *mut _),
                    &mut size,
                    std::mem::size_of::<RAWINPUTHEADER>() as u32,
                );
                if n != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE.0 {
                    s.mouse(raw.data.mouse);
                }
            }
            WM_SIZE => {
                let mut r = RECT::default();
                let _ = GetClientRect(hwnd, &mut r);
                if !s.video.get().is_invalid() {
                    let _ = SetWindowPos(
                        s.video.get(),
                        None,
                        0,
                        0,
                        r.right,
                        r.bottom,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                s.clip();
                return LRESULT(0);
            }
            WM_ACTIVATE => {
                if wp.0 as u16 == WA_INACTIVE as u16 {
                    s.release();
                } else if !s.view_only {
                    s.active.set(true);
                    s.clip();
                }
            }
            WM_KILLFOCUS => {
                s.release();
            }
            WM_FULLSCREEN => {
                s.fullscreen();
                return LRESULT(0);
            }
            WM_KEYDOWN if s.view_only && wp.0 == VK_F11.0 as usize => {
                if lp.0 & (1 << 30) == 0 {
                    s.fullscreen();
                }
                return LRESULT(0);
            }
            WM_SETCURSOR => {
                if (lp.0 & 0xffff) == HTCLIENT as isize {
                    s.apply_cursor();
                    return LRESULT(1);
                }
            }
            WM_DROPFILES => {
                let drop = HDROP(wp.0 as *mut _);
                let count = DragQueryFileW(drop, u32::MAX, None);
                let mut paths = vec![];
                for i in 0..count.min(1000) {
                    let len = DragQueryFileW(drop, i, None);
                    let mut path = vec![0; len as usize + 1];
                    DragQueryFileW(drop, i, Some(&mut path));
                    use std::os::windows::ffi::OsStringExt;
                    paths.push(PathBuf::from(std::ffi::OsString::from_wide(
                        &path[..len as usize],
                    )));
                }
                DragFinish(drop);
                s.send(InputEvent::ReleaseAll);
                s.mouse_buttons.set(0);
                let _ = s.file_tx.send(paths);
                return LRESULT(0);
            }
            WM_CLOSE => {
                s.release();
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            WM_DESTROY => {
                s.shared.alive.store(false, Ordering::Relaxed);
                let _ = KillTimer(Some(hwnd), VIDEO_TIMER);
                PostQuitMessage(0);
                return LRESULT(0);
            }
            WM_ERASEBKGND => return LRESULT(1),
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe extern "system" fn video_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_DROPFILES | WM_SETCURSOR => {
            if let Ok(parent) = GetParent(hwnd) {
                SendMessageW(parent, msg, Some(wp), Some(lp))
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn network(
    c: ViewerConfig,
    shared: Arc<Shared>,
    rx: Receiver<InputEvent>,
    events: Sender<NetworkEvent>,
) {
    while shared.alive.load(Ordering::Relaxed) {
        let result = (|| -> Result<()> {
            let (address, input_port) = if let Some(host) = &c.server {
                let address = (host.as_str(), c.port)
                    .to_socket_addrs()?
                    .next()
                    .context("server did not resolve")?;
                (address, c.input_port)
            } else {
                let Some(found) = discovery::discover(Duration::from_secs(2))? else {
                    return Ok(());
                };
                found
            };
            let mut stream = TcpStream::connect_timeout(
                &SocketAddr::new(address.ip(), input_port),
                Duration::from_secs(2),
            )?;
            stream.set_nodelay(true)?;
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            stream.set_write_timeout(Some(Duration::from_millis(500)))?;
            stream.write_all(MAGIC)?;
            let mut hello = [0u8; 36];
            stream.read_exact(&mut hello)?;
            ensure!(&hello[..8] == MAGIC, "server protocol mismatch");
            let width = u16::from_le_bytes(hello[24..26].try_into()?);
            let height = u16::from_le_bytes(hello[26..28].try_into()?);
            let fps = u16::from_le_bytes(hello[28..30].try_into()?);
            let source_w = i32::from_le_bytes(hello[16..20].try_into()?);
            let source_h = i32::from_le_bytes(hello[20..24].try_into()?);
            let port = u16::from_le_bytes(hello[30..32].try_into()?);
            let container = if u16::from_le_bytes(hello[34..36].try_into()?) == 1 {
                "nut"
            } else {
                "mpegts"
            };
            ensure!(
                width > 0 && height > 0 && fps > 0,
                "invalid server geometry"
            );
            // Flush events from any previous session, then explicitly reset all remote keys.
            while rx.try_recv().is_ok() {}
            stream.write_all(&InputEvent::ReleaseAll.encode())?;
            shared.connected.store(true, Ordering::Relaxed);
            let url = format!(
                "tcp://{}:{}?tcp_nodelay=1&timeout=3000000",
                address.ip(),
                port
            );
            let _ = events.send(NetworkEvent::Connected {
                url,
                width: width as i32,
                height: height as i32,
                fps,
                source_w: source_w.max(0) as usize,
                source_h: source_h.max(0) as usize,
                container,
            });
            let mut incoming = stream.try_clone()?;
            let connected = Arc::new(AtomicBool::new(true));
            let reader_alive = connected.clone();
            let reader_shared = shared.clone();
            let cursor_events = events.clone();
            let server_ip = address.ip();
            let reader = std::thread::spawn(move || {
                let mut buf = [0; 2];
                while reader_alive.load(Ordering::Relaxed)
                    && reader_shared.alive.load(Ordering::Relaxed)
                {
                    if incoming.read_exact(&mut buf).is_err() {
                        break;
                    }
                    if buf[0] == 0xC0 {
                        reader_shared.cursor.store(buf[1], Ordering::Relaxed);
                    } else if buf[0] == 0xC1 {
                        let mut packet = vec![];
                        let shape = read_packet(&mut incoming, &mut packet)
                            .and_then(|_| CursorShape::decode(&mut Reader::new(&packet)));
                        match shape {
                            Ok(shape) => {
                                let _ = cursor_events.send(NetworkEvent::Cursor(shape));
                            }
                            Err(_) => break,
                        }
                    } else if buf[0] == 0xC2 || buf[0] == 0xC3 {
                        let mut packet = vec![];
                        if read_packet(&mut incoming, &mut packet).is_err() {
                            break;
                        }
                        if buf[0] == 0xC2 {
                            if let Ok(info) = serde_json::from_slice::<ServerInfo>(&packet) {
                                *reader_shared.file_address.lock().unwrap() =
                                    Some(SocketAddr::new(server_ip, info.file_port));
                                let _ = cursor_events.send(NetworkEvent::Info(info));
                            }
                        } else if let Ok(stats) = serde_json::from_slice::<ServerStats>(&packet) {
                            let _ = cursor_events.send(NetworkEvent::Stats(stats));
                        }
                    } else {
                        break;
                    }
                }
                reader_alive.store(false, Ordering::Relaxed);
                let _ = incoming.shutdown(std::net::Shutdown::Both);
            });
            let result = (|| -> Result<()> {
                while connected.load(Ordering::Relaxed) && shared.alive.load(Ordering::Relaxed) {
                    let event = rx
                        .recv_timeout(Duration::from_millis(250))
                        .unwrap_or(InputEvent::Heartbeat);
                    // Combine only adjacent motion events; key/button ordering remains exact.
                    let mut pending = event;
                    let mut batch = Vec::with_capacity(512);
                    for next in rx.try_iter().take(127) {
                        match (pending, next) {
                            (InputEvent::Absolute { .. }, n @ InputEvent::Absolute { .. }) => {
                                pending = n
                            }
                            (
                                InputEvent::Relative { x, y },
                                InputEvent::Relative { x: dx, y: dy },
                            ) => {
                                pending = InputEvent::Relative {
                                    x: x.saturating_add(dx),
                                    y: y.saturating_add(dy),
                                }
                            }
                            _ => {
                                batch.extend_from_slice(&pending.encode());
                                pending = next;
                            }
                        }
                    }
                    batch.extend_from_slice(&pending.encode());
                    stream.write_all(&batch)?;
                }
                Ok(())
            })();
            let _ = stream.write_all(&InputEvent::ReleaseAll.encode());
            let _ = stream.shutdown(std::net::Shutdown::Both);
            connected.store(false, Ordering::Relaxed);
            let _ = reader.join();
            result
        })();
        shared.connected.store(false, Ordering::Relaxed);
        *shared.file_address.lock().unwrap() = None;
        if let Err(e) = result {
            log(format!("Connection: {e:#}"));
            let _ = events.send(NetworkEvent::Disconnected(e.to_string()));
        } else {
            let _ = events.send(NetworkEvent::Disconnected("Searching for server".into()));
        }
        for _ in 0..10 {
            if !shared.alive.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

pub fn run(c: ViewerConfig) -> Result<()> {
    if !c.send.is_empty() {
        let ip = if let Some(host) = &c.server {
            (host.as_str(), c.file_port)
                .to_socket_addrs()?
                .next()
                .context("server did not resolve")?
                .ip()
        } else {
            discovery::discover(Duration::from_secs(5))?
                .context("no server discovered")?
                .0
                .ip()
        };
        let names = crate::files::send(SocketAddr::new(ip, c.file_port), &c.send, |_, _, _| {})?;
        log(format!(
            "Sent {} file(s) to server Desktop: {}",
            names.len(),
            names.join(", ")
        ));
        return Ok(());
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _marker = if c.latency_probe {
            Some(crate::latency::Marker::start()?)
        } else {
            None
        };
        let module = GetModuleHandleW(None)?;
        let instance = HINSTANCE(module.0);
        let class = w!("ScreenerViewerNative");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        };
        ensure!(RegisterClassW(&wc) != 0, "registering viewer window");
        let video_class = w!("ScreenerVideoHost");
        let video_wc = WNDCLASSW {
            lpfnWndProc: Some(video_proc),
            hInstance: instance,
            lpszClassName: video_class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        ensure!(RegisterClassW(&video_wc) != 0, "registering video host");
        // Keep a local desktop cursor even on hosts without a physical mouse.
        for _ in 0..16 {
            if ShowCursor(true) >= 0 {
                break;
            }
        }
        let shared = Arc::new(Shared {
            alive: AtomicBool::new(true),
            connected: AtomicBool::new(false),
            cursor: AtomicU8::new(1),
            file_address: Mutex::new(None),
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (file_tx, file_rx) = crossbeam_channel::unbounded::<Vec<PathBuf>>();
        let s = Box::new(State {
            hwnd: Cell::new(HWND::default()),
            video: Cell::new(HWND::default()),
            tx,
            shared: shared.clone(),
            active: Cell::new(!c.view_only),
            relative: Cell::new(c.relative),
            view_only: c.view_only,
            fullscreen: Cell::new(false),
            old_rect: Cell::new(RECT::default()),
            vw: Cell::new(1920),
            vh: Cell::new(1080),
            hotkeys: Cell::new(0),
            modifiers: Cell::new(0),
            custom_cursor: RefCell::new(None),
            mouse_buttons: Cell::new(0),
            file_tx,
            playback: RefCell::new(None),
            playback_events: RefCell::new(Vec::new()),
            polling_video: Cell::new(false),
        });
        STATE.store(Box::as_ref(&s) as *const _ as *mut _, Ordering::Relaxed);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Screener · Connecting…"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            760,
            None,
            None,
            Some(instance),
            None,
        )?;
        s.hwnd.set(hwnd);
        let mut r = RECT::default();
        GetClientRect(hwnd, &mut r)?;
        // Disabled video host leaves keyboard/mouse ownership with our Rust window.
        let child = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            video_class,
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_DISABLED,
            0,
            0,
            r.right,
            r.bottom,
            Some(hwnd),
            None,
            Some(instance),
            None,
        )?;
        s.video.set(child);
        DragAcceptFiles(hwnd, true);
        DragAcceptFiles(child, true);
        let player = Rc::new(Player::new(child.0 as isize, c.legacy_buffering)?);
        *s.playback.borrow_mut() = Some(player.clone());
        ensure!(
            SetTimer(Some(hwnd), VIDEO_TIMER, 8, None) != 0,
            "video timer failed"
        );
        let devices = [RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 2,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        }];
        RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32)?;
        let hook = if c.view_only {
            None
        } else {
            Some(SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_hook),
                Some(instance),
                0,
            )?)
        };
        let network_shared = shared.clone();
        let network_cfg = c.clone();
        let transfer_shared = shared.clone();
        let transfer_events = event_tx.clone();
        let transfer_worker = std::thread::spawn(move || {
            while transfer_shared.alive.load(Ordering::Relaxed) {
                let Ok(paths) = file_rx.recv_timeout(Duration::from_millis(250)) else {
                    continue;
                };
                let address = *transfer_shared.file_address.lock().unwrap();
                let Some(address) = address else {
                    let _ = transfer_events.send(NetworkEvent::Transfer(
                        "Update/restart server.exe to enable file drops".into(),
                    ));
                    continue;
                };
                let mut update = Instant::now() - Duration::from_secs(1);
                let result = crate::files::send_cancellable(
                    address,
                    &paths,
                    |done, total, name| {
                        if update.elapsed() > Duration::from_millis(150) {
                            let pct = done.saturating_mul(100).checked_div(total).unwrap_or(100);
                            let _ = transfer_events
                                .send(NetworkEvent::Transfer(format!("Sending {name}: {pct}%")));
                            update = Instant::now();
                        }
                    },
                    || !transfer_shared.alive.load(Ordering::Relaxed),
                );
                let message = match result {
                    Ok(names) => format!("Sent {} file(s) to server Desktop", names.len()),
                    Err(e) => format!("File transfer failed: {e:#}"),
                };
                log(&message);
                let _ = transfer_events.send(NetworkEvent::Transfer(message));
            }
        });
        let worker = std::thread::spawn(move || network(network_cfg, network_shared, rx, event_tx));
        if c.fullscreen {
            s.fullscreen();
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let start = Instant::now();
        let mut msg = MSG::default();
        let mut quit = false;
        let mut retry_video: Option<Instant> = None;
        let mut url = String::new();
        let mut last_stats = Instant::now();
        let mut logged_hw = false;
        let mut frame_seen = false;
        let (mut source_w, mut source_h) = (2560usize, 1440usize);
        let mut ages = Vec::new();
        let mut transfer_status: Option<(String, Instant)> = None;
        let mut server_stats: Option<ServerStats> = None;
        let mut last_cursor = 254u8;
        let mut previous_viewer = ViewerStats::default();
        let mut previous_server = ServerStats::default();
        let mut modal_attempted = false;
        let mut modal_verified = !c.modal_probe;
        while !quit && shared.alive.load(Ordering::Relaxed) {
            // Do not let high-rate raw mouse messages starve video presentation.
            for _ in 0..128 {
                if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    break;
                }
                if msg.message == WM_QUIT {
                    quit = true;
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if quit || !shared.alive.load(Ordering::Relaxed) {
                break;
            }
            while let Ok(event) = event_rx.try_recv() {
                match event {
                    NetworkEvent::Connected {
                        url: new,
                        width,
                        height,
                        fps,
                        source_w: sw,
                        source_h: sh,
                        container,
                    } => {
                        s.vw.set(width);
                        source_w = sw;
                        source_h = sh;
                        s.vh.set(height);
                        s.active.set(!s.view_only && s.focused());
                        s.clip();
                        url = new;
                        logged_hw = false;
                        previous_viewer = ViewerStats::default();
                        previous_server = ServerStats::default();
                        server_stats = None;
                        log(format!(
                            "Connected video {width}x{height} @ {fps}fps: {url}"
                        ));
                        player.configure(width as u32, height as u32, fps, container)?;
                        player.load(&url)?;
                        retry_video = None;
                        let title=wide(&format!("Screener · {width}×{height} @ {fps} · F8 mouse · F11 fullscreen · Ctrl+Alt+F12 release"));
                        let _ = SetWindowTextW(hwnd, PCWSTR(title.as_ptr()));
                    }
                    NetworkEvent::Disconnected(reason) => {
                        s.release();
                        player.stop();
                        url.clear();
                        retry_video = None;
                        let title = wide(&format!("Screener · {reason} · Reconnecting…"));
                        let _ = SetWindowTextW(hwnd, PCWSTR(title.as_ptr()));
                    }
                    NetworkEvent::Cursor(shape) => {
                        match crate::cursor::NativeCursor::from_shape(&shape) {
                            Ok(cursor) => {
                                let old = s.custom_cursor.borrow_mut().replace(cursor);
                                s.apply_cursor();
                                drop(old);
                            }
                            Err(e) => log(format!("Cursor: {e:#}")),
                        }
                    }
                    NetworkEvent::Info(info) => {
                        log(format!(
                            "SERVER {} build={} GPU={} encoder={} codec={} rate={} QP={} path={} files={}",
                            info.machine,
                            info.build,
                            info.adapter,
                            info.encoder,
                            info.codec,
                            info.rate_control,
                            info.qp,
                            info.pipeline,
                            info.file_port
                        ));
                    }
                    NetworkEvent::Stats(stats) => {
                        server_stats = Some(stats);
                    }
                    NetworkEvent::Transfer(message) => {
                        let title = wide(&format!("Screener · {message}"));
                        let _ = SetWindowTextW(hwnd, PCWSTR(title.as_ptr()));
                        transfer_status = Some((message, Instant::now()));
                    }
                }
            }
            s.poll_video();
            let playback_events = std::mem::take(&mut *s.playback_events.borrow_mut());
            for (id, error) in playback_events {
                if id == 7 {
                    // MPV_EVENT_END_FILE
                    log(format!("Video ended (error {error}); reconnecting"));
                    if !url.is_empty() {
                        retry_video = Some(Instant::now() + Duration::from_millis(500));
                    }
                }
            }
            let cursor = s.shared.cursor.load(Ordering::Relaxed);
            if cursor != last_cursor {
                last_cursor = cursor;
                s.apply_cursor();
            }
            if retry_video.is_some_and(|t| Instant::now() >= t) {
                if shared.connected.load(Ordering::Relaxed) {
                    player.load(&url)?;
                }
                retry_video = None;
            }
            if last_stats.elapsed() > Duration::from_secs(1) {
                if let Some(stats) = player.stats() {
                    let dt = stats
                        .elapsed_ms
                        .saturating_sub(previous_viewer.elapsed_ms)
                        .max(1) as f64
                        / 1000.0;
                    let decoded = stats.decoded.saturating_sub(previous_viewer.decoded) as f64 / dt;
                    let shown =
                        stats.presented.saturating_sub(previous_viewer.presented) as f64 / dt;
                    let skips = stats.skipped.saturating_sub(previous_viewer.skipped);
                    let rendered = stats
                        .presented
                        .saturating_sub(previous_viewer.presented)
                        .max(1) as f64;
                    let render_ms = stats.render_us.saturating_sub(previous_viewer.render_us)
                        as f64
                        / rendered
                        / 1000.0;
                    let mut extra = String::new();
                    if let Some(server) = &server_stats {
                        let st = server
                            .elapsed_ms
                            .saturating_sub(previous_server.elapsed_ms)
                            .max(1) as f64
                            / 1000.0;
                        let encoded =
                            server.encoded.saturating_sub(previous_server.encoded) as f64 / st;
                        let captured =
                            server.captured.saturating_sub(previous_server.captured) as f64 / st;
                        let submitted =
                            server.submitted.saturating_sub(previous_server.submitted) as f64 / st;
                        let mbps = server.bytes.saturating_sub(previous_server.bytes) as f64 * 8.0
                            / st
                            / 1_000_000.0;
                        let blocked = server
                            .send_wait_us
                            .saturating_sub(previous_server.send_wait_us)
                            as f64
                            / 1000.0;
                        extra = format!(
                            " capture_fps={captured:.1} submitted_fps={submitted:.1} encoded_fps={encoded:.1} Mbps={mbps:.2} send_wait_ms={blocked:.1}"
                        );
                        previous_server = server.clone();
                    }
                    log(format!("PERF decoded_fps={decoded:.1} displayed_fps={shown:.1} skipped={skips} render_ms={render_ms:.2}{extra}"));
                    let status = transfer_status
                        .as_ref()
                        .filter(|(_, t)| t.elapsed() < Duration::from_secs(8))
                        .map(|(m, _)| format!(" · {m}"))
                        .unwrap_or_default();
                    let title=wide(&format!("Screener · {shown:.0} FPS · received {decoded:.0} · skipped {skips}{status} · F8 mouse · F11 fullscreen"));
                    let _ = SetWindowTextW(hwnd, PCWSTR(title.as_ptr()));
                    previous_viewer = stats;
                }
                if let Some(hw) = player.property("hwdec-current") {
                    if !hw.is_empty() && hw != "no" && !logged_hw {
                        log(format!("Hardware decoder active: {hw}"));
                        logged_hw = true;
                    }
                }
                if let Some(pts) = player.property("time-pos/full") {
                    log(format!(
                        "Video time={pts}; hw={}",
                        player.property("hwdec-current").unwrap_or_default()
                    ));
                    frame_seen = true;
                    if c.latency_probe && start.elapsed() > Duration::from_secs(3) {
                        match player.frame_age(source_w, source_h) {
                            Ok((age, cost)) => {
                                ages.push(age);
                                log(format!("LATENCY frame_age_ms={age} readback_ms={cost} demux_queue={} video_sync={}",player.property("demuxer-cache-duration").unwrap_or_default(),player.property("video-sync").unwrap_or_default()));
                            }
                            Err(e) => log(format!("LATENCY unavailable: {e:#}")),
                        }
                    }
                }
                last_stats = Instant::now();
            }
            if c.smoke_seconds
                .is_some_and(|t| start.elapsed().as_secs() >= t)
            {
                log(format!(
                    "SMOKE: connected={}, hardware_decode={logged_hw}, frame_seen={frame_seen}",
                    shared.connected.load(Ordering::Relaxed)
                ));
                if !frame_seen || !logged_hw {
                    log("SMOKE FAILED: expected decoded GPU frames");
                }
                let _ = DestroyWindow(hwnd);
                break;
            }
            if c.modal_probe && !modal_attempted && start.elapsed() > Duration::from_secs(5) {
                if let Some(before) = player.stats() {
                    modal_attempted = true;
                    let modal_start = Instant::now();
                    // Exercise a nested message loop with no outer render poll,
                    // just as DefWindowProc does during interactive move/resize.
                    while modal_start.elapsed() < Duration::from_secs(4)
                        && shared.alive.load(Ordering::Relaxed)
                    {
                        for _ in 0..128 {
                            if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                                break;
                            }
                            if msg.message == WM_QUIT {
                                quit = true;
                                break;
                            }
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    if let Some(after) = player.stats() {
                        let decoded = after.decoded.saturating_sub(before.decoded);
                        let shown = after.presented.saturating_sub(before.presented);
                        modal_verified = decoded >= 30 && shown * 100 >= decoded * 85;
                        log(format!("MODAL PROBE: decoded={decoded} presented={shown} passed={modal_verified}"));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        s.release();
        if !ages.is_empty() {
            ages.sort_unstable();
            log(format!(
                "LATENCY SUMMARY samples={} min_ms={} median_ms={} max_ms={}",
                ages.len(),
                ages[0],
                ages[ages.len() / 2],
                ages[ages.len() - 1]
            ));
        }
        shared.alive.store(false, Ordering::Relaxed);
        let _ = KillTimer(Some(hwnd), VIDEO_TIMER);
        if let Some(hook) = hook {
            let _ = UnhookWindowsHookEx(hook);
        }
        STATE.store(std::ptr::null_mut(), Ordering::Relaxed);
        s.playback.borrow_mut().take();
        drop(player);
        let _ = worker.join();
        let _ = transfer_worker.join();
        if c.smoke_seconds.is_some() {
            ensure!(
                frame_seen && logged_hw,
                "smoke test did not decode hardware video"
            );
        }
        ensure!(
            modal_verified,
            "video did not keep presenting in nested message loop"
        );
        Ok(())
    }
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
