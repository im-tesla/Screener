//! Follow the desktop that currently receives user input.
//!
//! Windows keeps UAC prompts, the lock screen, and the security screen on the
//! `Winlogon` secure desktop. Only a LocalSystem process can open that desktop,
//! and capture/input calls apply to the calling thread's desktop, so the
//! service-launched worker switches its capture and input threads to whichever
//! desktop is active. On the normal desktop this is a no-op.
use anyhow::{Context, Result};
use std::cell::RefCell;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::HANDLE,
        System::{
            StationsAndDesktops::{
                CloseDesktop, GetUserObjectInformationW, OpenDesktopW, OpenInputDesktop,
                SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
            },
            Threading::GetCurrentThreadId,
        },
    },
};

/// `GENERIC_ALL`, the mask desktop-aware input injectors request. A narrower
/// mask still allows capture but leaves the attached thread unable to inject
/// input: SendInput returns ERROR_ACCESS_DENIED even on the default desktop.
const ACCESS: DESKTOP_ACCESS_FLAGS = DESKTOP_ACCESS_FLAGS(0x1000_0000);

thread_local! {
    /// Every server thread starts on the process's startup desktop (`Default`).
    static CURRENT: RefCell<String> = RefCell::new("Default".to_string());
    /// The open handle for the desktop this thread is attached to. It is kept
    /// open for as long as the thread uses the desktop and closed only after a
    /// later switch, matching the documented SetThreadDesktop pattern.
    static HELD: RefCell<Option<HDESK>> = const { RefCell::new(None) };
}

pub fn current() -> String {
    CURRENT.with(|name| name.borrow().clone())
}

/// Name of the desktop currently receiving input, without attaching this thread.
pub fn input_name() -> Result<String> {
    unsafe {
        let input = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, ACCESS)
            .context("opening the input desktop")?;
        let name = name_of(input);
        let _ = CloseDesktop(input);
        name
    }
}

/// Attach the calling thread to a named desktop on the process's window station.
/// This must be the thread's first User32 operation: a thread that has already
/// created a window, hook, or input queue does not move its input target with
/// SetThreadDesktop, which makes SendInput fail with access denied. The handle is
/// held while the thread is attached.
pub fn attach(name: &str) -> Result<()> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let desktop = OpenDesktopW(
            PCWSTR(wide.as_ptr()),
            DESKTOP_CONTROL_FLAGS(0),
            false,
            ACCESS.0,
        )
        .with_context(|| format!("opening desktop {name}"))?;
        if let Err(e) = SetThreadDesktop(desktop) {
            let _ = CloseDesktop(desktop);
            return Err(e).with_context(|| format!("attaching this thread to desktop {name}"));
        }
        if let Some(previous) = HELD.with(|held| held.replace(Some(desktop))) {
            let _ = CloseDesktop(previous);
        }
        crate::util::log(format!(
            "Desktop attach: thread {} -> {name}",
            GetCurrentThreadId()
        ));
        CURRENT.with(|current| *current.borrow_mut() = name.to_string());
        Ok(())
    }
}

fn name_of(desktop: HDESK) -> Result<String> {
    unsafe {
        let mut buffer = [0u16; 128];
        let mut needed = 0u32;
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buffer.as_mut_ptr() as *mut _),
            std::mem::size_of_val(&buffer) as u32,
            Some(&mut needed),
        )
        .context("reading the desktop name")?;
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        Ok(String::from_utf16_lossy(&buffer[..len]))
    }
}

/// Switch the calling thread to the desktop receiving input. Returns whether the
/// thread's desktop changed. The thread must not own windows or hooks; the
/// capture and input threads are plain worker threads, so this holds.
pub fn follow_input_desktop() -> Result<bool> {
    unsafe {
        let input = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, ACCESS)
            .context("opening the input desktop")?;
        let name = match name_of(input) {
            Ok(name) => name,
            Err(e) => {
                let _ = CloseDesktop(input);
                return Err(e);
            }
        };
        if CURRENT.with(|current| *current.borrow() == name) {
            let _ = CloseDesktop(input);
            return Ok(false);
        }
        let switched = SetThreadDesktop(input);
        if let Err(e) = switched {
            let _ = CloseDesktop(input);
            return Err(e).with_context(|| format!("switching this thread to desktop {name}"));
        }
        // Hold the handle while the thread is attached to this desktop.
        if let Some(previous) = HELD.with(|held| held.replace(Some(input))) {
            let _ = CloseDesktop(previous);
        }
        crate::util::log(format!(
            "Desktop switch: thread {} -> {name}",
            GetCurrentThreadId()
        ));
        CURRENT.with(|current| *current.borrow_mut() = name);
        Ok(true)
    }
}
