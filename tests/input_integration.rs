//! Opt-in real Windows input test. Hooks swallow only Screener-tagged test
//! injections, so no test keystrokes/clicks reach the user's foreground app.
use screener::{
    input::INJECTION_TAG,
    protocol::{InputEvent, MAGIC},
};
use std::{
    io::{Read, Write},
    net::TcpStream,
    os::windows::process::CommandExt,
    process::{Child, Command},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::*, System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*,
};

static KEYS: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());
static MOUSE: Mutex<Vec<u32>> = Mutex::new(Vec::new());
unsafe extern "system" fn key_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        if k.dwExtraInfo == INJECTION_TAG {
            KEYS.lock().unwrap().push((k.vkCode, k.flags.0));
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}
unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let m = &*(lp.0 as *const MSLLHOOKSTRUCT);
        if m.dwExtraInfo == INJECTION_TAG {
            MOUSE.lock().unwrap().push(wp.0 as u32);
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}
struct Hooks(HHOOK, HHOOK);
impl Drop for Hooks {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
            let _ = UnhookWindowsHookEx(self.1);
        }
    }
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "needs an unlocked Windows desktop and dist/server.exe; all tagged injections are intercepted"]
fn tcp_input_injection_and_disconnect_release() {
    KEYS.lock().unwrap().clear();
    MOUSE.lock().unwrap().clear();
    let module = unsafe { GetModuleHandleW(None).unwrap() };
    let _hooks = unsafe {
        Hooks(
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), Some(HINSTANCE(module.0)), 0)
                .unwrap(),
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(HINSTANCE(module.0)), 0).unwrap(),
        )
    };
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dist/server.exe");
    let _server = Server(
        Command::new(path)
            .args([
                "--bind",
                "127.0.0.1",
                "--port",
                "48090",
                "--input-port",
                "48091",
                "--smoke-seconds",
                "12",
            ])
            .creation_flags(0x08000000)
            .spawn()
            .unwrap(),
    );
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let worker_done = done.clone();
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(7);
        let mut stream = loop {
            match TcpStream::connect("127.0.0.1:48091") {
                Ok(s) => break s,
                Err(e) => {
                    assert!(Instant::now() < deadline, "input server did not start: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream.write_all(MAGIC).unwrap();
        let mut hello = [0; 36];
        stream.read_exact(&mut hello).unwrap();
        assert_eq!(&hello[..8], MAGIC);
        for event in [
            InputEvent::Key {
                scan: 0x1e,
                vk: 0x41,
                flags: 0,
            },
            InputEvent::Key {
                scan: 0x1e,
                vk: 0x41,
                flags: 1,
            },
            InputEvent::Key {
                scan: 0x1d,
                vk: 0xa3,
                flags: 2,
            },
            InputEvent::Key {
                scan: 0x1d,
                vk: 0xa3,
                flags: 3,
            },
            InputEvent::Key {
                scan: 0x1c,
                vk: 0x0d,
                flags: 2,
            },
            InputEvent::Key {
                scan: 0x1c,
                vk: 0x0d,
                flags: 3,
            },
            InputEvent::Key {
                scan: 0x45,
                vk: 0x13,
                flags: 4,
            },
            InputEvent::Key {
                scan: 0x45,
                vk: 0x13,
                flags: 5,
            },
            InputEvent::Absolute { x: 32768, y: 32768 },
            InputEvent::Relative { x: 3, y: -4 },
            InputEvent::Button {
                button: 0,
                down: true,
            },
            InputEvent::Button {
                button: 0,
                down: false,
            },
            InputEvent::Button {
                button: 4,
                down: true,
            },
            InputEvent::Button {
                button: 4,
                down: false,
            },
            InputEvent::Wheel {
                delta: 120,
                horizontal: false,
            },
            InputEvent::Wheel {
                delta: -120,
                horizontal: true,
            },
            // Intentionally leave these held, then drop TCP: server must release them.
            InputEvent::Key {
                scan: 0x2a,
                vk: 0xa0,
                flags: 0,
            },
            InputEvent::Button {
                button: 1,
                down: true,
            },
        ] {
            stream.write_all(&event.encode()).unwrap();
        }
        // Let the server process the queue before disconnecting its duplex socket.
        std::thread::sleep(Duration::from_millis(200));
        drop(stream);
        std::thread::sleep(Duration::from_millis(200));
        worker_done.store(true, Ordering::Relaxed);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut msg = MSG::default();
    while Instant::now() < deadline {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if done.load(Ordering::Relaxed)
            && KEYS
                .lock()
                .unwrap()
                .iter()
                .any(|(vk, f)| *vk == 0xa0 && f & LLKHF_UP.0 != 0)
            && MOUSE.lock().unwrap().contains(&WM_RBUTTONUP)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    worker.join().unwrap();
    let keys = KEYS.lock().unwrap();
    let mouse = MOUSE.lock().unwrap();
    assert!(
        keys.iter()
            .any(|(vk, f)| *vk == 0x41 && f & LLKHF_UP.0 == 0),
        "A down missing: {keys:?}"
    );
    assert!(
        keys.iter()
            .any(|(vk, f)| *vk == 0x41 && f & LLKHF_UP.0 != 0),
        "A up missing: {keys:?}"
    );
    assert!(
        keys.iter()
            .any(|(vk, f)| *vk == 0xa3 && f & LLKHF_EXTENDED.0 != 0),
        "extended right Ctrl missing: {keys:?}"
    );
    assert!(
        keys.iter()
            .any(|(vk, f)| *vk == 0x0d && f & LLKHF_EXTENDED.0 != 0),
        "numpad Enter missing: {keys:?}"
    );
    assert!(
        keys.iter().any(|(vk, _)| *vk == 0x13),
        "Pause missing: {keys:?}"
    );
    assert!(
        keys.iter()
            .any(|(vk, f)| *vk == 0xa0 && f & LLKHF_UP.0 != 0),
        "disconnect did not release Shift: {keys:?}"
    );
    for message in [
        WM_MOUSEMOVE,
        WM_LBUTTONDOWN,
        WM_LBUTTONUP,
        WM_XBUTTONDOWN,
        WM_XBUTTONUP,
        WM_MOUSEWHEEL,
        WM_MOUSEHWHEEL,
        WM_RBUTTONDOWN,
        WM_RBUTTONUP,
    ] {
        assert!(
            mouse.contains(&message),
            "mouse message {message:x} missing: {mouse:?}"
        );
    }
    println!(
        "PASS: {} keyboard and {} mouse hook events, including disconnect releases",
        keys.len(),
        mouse.len()
    );
}
