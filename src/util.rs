use std::{
    fs::OpenOptions,
    io::Write,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);
static RUNTIME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
static RUNTIME_ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn bundled_runtime() -> Option<&'static std::path::Path> {
    RUNTIME.get().map(|p| p.as_path())
}

/// The service worker unpacks beside its protected Program Files executable
/// instead of into a user-writable profile cache.
pub fn set_runtime_root(path: std::path::PathBuf) {
    let _ = RUNTIME_ROOT.set(path);
}

/// Unpack only on first use/version change. A content-addressed cache lets the
/// two executables remain independently portable with no installer or prompts.
pub fn prepare_runtime(name: &str, packed: &[u8], hash: &str, size: &str) -> anyhow::Result<()> {
    use anyhow::{ensure, Context};
    use sha2::{Digest, Sha256};
    use std::{
        fs::{self, File},
        io::{BufReader, Read},
        path::PathBuf,
    };
    let root = RUNTIME_ROOT
        .get()
        .cloned()
        .unwrap_or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join("Screener")
                .join("runtime")
        })
        .join(&hash[..16]);
    fs::create_dir_all(&root)?;
    let path = root.join(name);
    let expected: u64 = size.parse()?;
    let valid = (|| -> anyhow::Result<bool> {
        if fs::metadata(&path)?.len() != expected {
            return Ok(false);
        }
        let mut hasher = Sha256::new();
        let mut file = BufReader::new(File::open(&path)?);
        let mut buf = [0; 65536];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()) == hash)
    })()
    .unwrap_or(false);
    if !valid {
        let temp = root.join(format!("{name}.{}.tmp", std::process::id()));
        zstd::stream::copy_decode(packed, File::create(&temp)?)
            .context("unpacking video runtime")?;
        ensure!(
            fs::metadata(&temp)?.len() == expected,
            "video runtime size mismatch"
        );
        if let Err(e) = fs::rename(&temp, &path) {
            // Another instance may have completed the same extraction first.
            if fs::metadata(&path).map(|m| m.len()).ok() != Some(expected) {
                return Err(e.into());
            }
            let _ = fs::remove_file(&temp);
        }
    }
    let _ = RUNTIME.set(path);
    Ok(())
}

pub fn init_log(name: &str) {
    let path = std::env::current_exe()
        .unwrap_or_default()
        .with_file_name(format!("{name}.log"));
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    *LOG.lock().unwrap() = file;
    std::panic::set_hook(Box::new(|info| log(format!("PANIC: {info}"))));
}

pub fn log(message: impl std::fmt::Display) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    if let Some(file) = LOG.lock().unwrap().as_mut() {
        let _ = writeln!(
            file,
            "{}.{:03} {message}",
            now.as_secs(),
            now.subsec_millis()
        );
    }
    eprintln!("{message}");
}

pub fn private_peer(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
        }
    }
}

pub struct TimerResolution;
impl TimerResolution {
    pub fn new() -> Self {
        unsafe {
            windows::Win32::Media::timeBeginPeriod(1);
        }
        Self
    }
}
impl Default for TimerResolution {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::Media::timeEndPeriod(1);
        }
    }
}
