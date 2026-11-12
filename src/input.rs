use crate::protocol::InputEvent;
use anyhow::{ensure, Result};
use std::collections::HashSet;
use windows::Win32::UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*};

/// Identifies our injected input, allowing local diagnostics without a feedback loop.
pub const INJECTION_TAG: usize = 0x5343524E;

#[derive(Clone, Copy, Debug, Default)]
pub struct DesktopRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

pub struct Injector {
    pub rect: DesktopRect,
    keys: HashSet<(u16, u16, u8)>,
    buttons: HashSet<u8>,
}
impl Injector {
    pub fn new(rect: DesktopRect) -> Self {
        Self {
            rect,
            keys: HashSet::new(),
            buttons: HashSet::new(),
        }
    }
    pub fn apply(&mut self, event: InputEvent) -> Result<()> {
        let input = match event {
            InputEvent::Key { scan, vk, flags } => {
                let key = (scan, vk, flags & 6);
                if flags & 1 == 0 {
                    self.keys.insert(key);
                } else {
                    self.keys.remove(&key);
                }
                // Pause uses the virtual key; its E1 prefix is not an E0 extended scan code.
                let mut f = KEYBD_EVENT_FLAGS(0);
                let virtual_key = scan == 0 || flags & 4 != 0 || vk == VK_PAUSE.0;
                if !virtual_key {
                    f |= KEYEVENTF_SCANCODE;
                }
                if flags & 1 != 0 {
                    f |= KEYEVENTF_KEYUP;
                }
                if flags & 2 != 0 {
                    f |= KEYEVENTF_EXTENDEDKEY;
                }
                INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: if virtual_key {
                                VIRTUAL_KEY(vk)
                            } else {
                                VIRTUAL_KEY(0)
                            },
                            wScan: if virtual_key { 0 } else { scan },
                            dwFlags: f,
                            dwExtraInfo: INJECTION_TAG,
                            ..Default::default()
                        },
                    },
                }
            }
            InputEvent::Absolute { x, y } => {
                let vx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
                let vy = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
                let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(2);
                let vh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(2);
                let px =
                    self.rect.left as i64 + x as i64 * (self.rect.width - 1).max(0) as i64 / 65535;
                let py =
                    self.rect.top as i64 + y as i64 * (self.rect.height - 1).max(0) as i64 / 65535;
                mouse(
                    ((px - vx as i64) * 65535 / (vw - 1) as i64) as i32,
                    ((py - vy as i64) * 65535 / (vh - 1) as i64) as i32,
                    0,
                    MOUSEEVENTF_MOVE
                        | MOUSEEVENTF_ABSOLUTE
                        | MOUSEEVENTF_VIRTUALDESK
                        | MOUSEEVENTF_MOVE_NOCOALESCE,
                )
            }
            InputEvent::Relative { x, y } => {
                mouse(x, y, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE)
            }
            InputEvent::Button { button, down } => {
                if down {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
                let (f, data) = button_flag(button, down);
                mouse(0, 0, data, f)
            }
            InputEvent::Wheel { delta, horizontal } => mouse(
                0,
                0,
                delta as u32,
                if horizontal {
                    MOUSEEVENTF_HWHEEL
                } else {
                    MOUSEEVENTF_WHEEL
                },
            ),
            InputEvent::ReleaseAll => {
                self.release_all();
                return Ok(());
            }
            InputEvent::Heartbeat => return Ok(()),
        };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        ensure!(
            sent == 1,
            "SendInput rejected event (target may be elevated or on a secure desktop)"
        );
        Ok(())
    }
    pub fn release_all(&mut self) {
        let keys: Vec<_> = self.keys.drain().collect();
        let buttons: Vec<_> = self.buttons.drain().collect();
        for (scan, vk, flags) in keys {
            let _ = self.apply(InputEvent::Key {
                scan,
                vk,
                flags: flags | 1,
            });
        }
        for button in buttons {
            let _ = self.apply(InputEvent::Button {
                button,
                down: false,
            });
        }
    }
}
impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}
fn mouse(x: i32, y: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: x,
                dy: y,
                mouseData: data,
                dwFlags: flags,
                dwExtraInfo: INJECTION_TAG,
                ..Default::default()
            },
        },
    }
}
fn button_flag(b: u8, down: bool) -> (MOUSE_EVENT_FLAGS, u32) {
    match (b, down) {
        (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (0, false) => (MOUSEEVENTF_LEFTUP, 0),
        (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (3, _) => (
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON1 as u32,
        ),
        _ => (
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON2 as u32,
        ),
    }
}

// Match system cursor handles so common text/resize/hand cursors stay responsive
// at the viewer's native mouse sampling rate, independently of the video frame.
pub fn cursor_id() -> u8 {
    current_cursor().0
}
pub fn current_cursor() -> (u8, usize) {
    unsafe {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut info).is_err() || info.hCursor.is_invalid() {
            return (0, 0);
        }
        // A headless host may suppress drawing but still report its current shape.
        // Send that shape; the viewer owns desktop pointer visibility.
        for (i, id) in cursor_ids().iter().enumerate() {
            if let Ok(c) = LoadCursorW(None, *id) {
                if c == info.hCursor {
                    return (i as u8 + 1, info.hCursor.0 as usize);
                }
            }
        }
        (255, info.hCursor.0 as usize)
    }
}
pub fn cursor_ids() -> [windows::core::PCWSTR; 14] {
    [
        IDC_ARROW,
        IDC_IBEAM,
        IDC_WAIT,
        IDC_CROSS,
        IDC_UPARROW,
        IDC_SIZE,
        IDC_ICON,
        IDC_SIZENWSE,
        IDC_SIZENESW,
        IDC_SIZEWE,
        IDC_SIZENS,
        IDC_SIZEALL,
        IDC_NO,
        IDC_HAND,
    ]
}

pub fn map_point(
    x: i32,
    y: i32,
    client_w: i32,
    client_h: i32,
    video_w: i32,
    video_h: i32,
) -> Option<(u16, u16)> {
    map_point_impl(x, y, client_w, client_h, video_w, video_h, false)
}
pub fn map_point_clamped(x: i32, y: i32, cw: i32, ch: i32, vw: i32, vh: i32) -> Option<(u16, u16)> {
    map_point_impl(x, y, cw, ch, vw, vh, true)
}
fn map_point_impl(
    mut x: i32,
    mut y: i32,
    client_w: i32,
    client_h: i32,
    video_w: i32,
    video_h: i32,
    clamp: bool,
) -> Option<(u16, u16)> {
    if client_w <= 0 || client_h <= 0 || video_w <= 0 || video_h <= 0 {
        return None;
    }
    let scale = (client_w as f64 / video_w as f64).min(client_h as f64 / video_h as f64);
    let w = (video_w as f64 * scale).round() as i32;
    let h = (video_h as f64 * scale).round() as i32;
    let left = (client_w - w) / 2;
    let top = (client_h - h) / 2;
    if clamp {
        x = x.clamp(left, left + w - 1);
        y = y.clamp(top, top + h - 1);
    }
    if x < left || y < top || x >= left + w || y >= top + h {
        return None;
    }
    Some((
        ((x - left) as i64 * 65535 / (w - 1).max(1) as i64) as u16,
        ((y - top) as i64 * 65535 / (h - 1).max(1) as i64) as u16,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapping_handles_letterbox_and_edges() {
        assert_eq!(map_point(0, 0, 1920, 1080, 1920, 1080), Some((0, 0)));
        assert_eq!(
            map_point(1919, 1079, 1920, 1080, 1920, 1080),
            Some((65535, 65535))
        );
        assert_eq!(map_point(0, 0, 1000, 1000, 1920, 1080), None);
        assert_eq!(map_point(500, 500, 0, 0, 1920, 1080), None);
        assert!(map_point(500, 500, 1000, 1000, 1920, 1080).is_some());
        assert_eq!(
            map_point_clamped(-200, 2000, 1920, 1080, 1920, 1080),
            Some((0, 65535))
        );
    }
}
