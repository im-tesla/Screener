//! File copies use their own TCP connection and are committed to Desktop only
//! after complete reception and SHA-256 verification. Existing files are kept.
use crate::{
    protocol::{read_packet, write_packet},
    util::{log, private_peer},
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Storage::FileSystem::MoveFileW,
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_Desktop, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG},
    },
};

const FILE_MAGIC: &[u8; 8] = b"SCRFILE1";
const CHUNK: usize = 64 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
#[derive(Serialize, Deserialize)]
struct Header {
    name: String,
    size: u64,
}
#[derive(Serialize, Deserialize)]
struct Reply {
    ok: bool,
    message: String,
}

pub fn desktop() -> Result<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Desktop, KNOWN_FOLDER_FLAG(0), None)?;
        let path = PathBuf::from(p.to_string()?);
        CoTaskMemFree(Some(p.0.cast()));
        Ok(path)
    }
}
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.encode_utf16().count() <= 240
            && !name.ends_with(['.', ' '])
            && !name.chars().any(|c| c < ' ' || "/\\:*?\"<>|".contains(c)),
        "invalid file name"
    );
    ensure!(name != "." && name != "..", "invalid file name");
    let base = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    ensure!(
        !["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&base.as_str())
            && !((base.starts_with("COM") || base.starts_with("LPT"))
                && base.len() == 4
                && matches!(base.as_bytes()[3], b'1'..=b'9')),
        "reserved file name"
    );
    Ok(())
}
fn reply(stream: &mut TcpStream, ok: bool, message: impl Into<String>) -> Result<()> {
    write_packet(
        stream,
        &serde_json::to_vec(&Reply {
            ok,
            message: message.into(),
        })?,
    )
}
fn receive_reply(stream: &mut TcpStream) -> Result<String> {
    let mut data = vec![];
    read_packet(stream, &mut data)?;
    let r: Reply = serde_json::from_slice(&data)?;
    ensure!(r.ok, "{}", r.message);
    Ok(r.message)
}
struct Partial(PathBuf);
impl Drop for Partial {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn commit(temp: &Path, dest: &Path, name: &str) -> Result<PathBuf> {
    let source = Path::new(name);
    let stem = source.file_stem().and_then(|v| v.to_str()).unwrap_or(name);
    let ext = source.extension().and_then(|v| v.to_str());
    for index in 0..10000 {
        let file = if index == 0 {
            name.to_string()
        } else {
            format!(
                "{stem} ({}){}",
                index + 1,
                ext.map(|e| format!(".{e}")).unwrap_or_default()
            )
        };
        let final_path = dest.join(file);
        let from = wide(temp);
        let to = wide(&final_path);
        match unsafe { MoveFileW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr())) } {
            Ok(()) => return Ok(final_path),
            Err(_) if final_path.exists() => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("too many files with that name")
}
pub fn receive(mut stream: TcpStream, destination: &Path) -> Result<Vec<PathBuf>> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut magic = [0; 8];
    stream.read_exact(&mut magic)?;
    ensure!(&magic == FILE_MAGIC, "file protocol mismatch");
    let mut count = [0; 4];
    stream.read_exact(&mut count)?;
    let count = u32::from_le_bytes(count);
    ensure!((1..=1000).contains(&count), "invalid file count");
    fs::create_dir_all(destination)?;
    let mut received = vec![];
    let mut buffer = vec![0; CHUNK];
    for _ in 0..count {
        let mut data = vec![];
        read_packet(&mut stream, &mut data)?;
        let header: Header = serde_json::from_slice(&data)?;
        if let Err(e) = validate_name(&header.name) {
            let _ = reply(&mut stream, false, e.to_string());
            return Err(e);
        }
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp = Partial(destination.join(format!(".screener-{}-{id}.part", std::process::id())));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp.0)
        {
            Ok(f) => f,
            Err(e) => {
                let _ = reply(&mut stream, false, e.to_string());
                return Err(e.into());
            }
        };
        reply(&mut stream, true, "ready")?;
        let mut remaining = header.size;
        let mut digest = Sha256::new();
        let reception = (|| -> Result<()> {
            while remaining > 0 {
                let n = remaining.min(CHUNK as u64) as usize;
                stream.read_exact(&mut buffer[..n])?;
                file.write_all(&buffer[..n])?;
                digest.update(&buffer[..n]);
                remaining -= n as u64;
            }
            let mut expected = [0; 32];
            stream.read_exact(&mut expected)?;
            let actual: [u8; 32] = digest.finalize().into();
            ensure!(actual == expected, "SHA-256 mismatch");
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        if let Err(e) = reception {
            let _ = reply(&mut stream, false, e.to_string());
            return Err(e);
        }
        match commit(&temp.0, destination, &header.name) {
            Ok(path) => {
                reply(
                    &mut stream,
                    true,
                    path.file_name().unwrap().to_string_lossy(),
                )?;
                log(format!(
                    "Received file {} ({} bytes, SHA-256 verified)",
                    path.display(),
                    header.size
                ));
                received.push(path);
            }
            Err(e) => {
                let _ = reply(&mut stream, false, e.to_string());
                return Err(e);
            }
        }
    }
    Ok(received)
}
pub fn listen(bind: &str, port: u16, alive: Arc<AtomicBool>) -> Result<()> {
    let listener = TcpListener::bind((bind, port)).context("binding file-transfer port")?;
    listener.set_nonblocking(true)?;
    std::thread::spawn(move || {
        while alive.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, peer)) if private_peer(peer.ip()) => {
                    match desktop().and_then(|dest| receive(stream, &dest)) {
                        Ok(_) => {}
                        Err(e) => log(format!("File receive: {e:#}")),
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(e) => {
                    log(format!("File listener: {e}"));
                    break;
                }
            }
        }
    });
    Ok(())
}

pub fn send(
    address: SocketAddr,
    paths: &[PathBuf],
    progress: impl FnMut(u64, u64, &str),
) -> Result<Vec<String>> {
    send_cancellable(address, paths, progress, || false)
}
pub fn send_cancellable(
    address: SocketAddr,
    paths: &[PathBuf],
    mut progress: impl FnMut(u64, u64, &str),
    mut cancelled: impl FnMut() -> bool,
) -> Result<Vec<String>> {
    ensure!(
        !paths.is_empty() && paths.len() <= 1000,
        "drop between 1 and 1000 files"
    );
    let mut files = vec![];
    let mut total = 0u64;
    for path in paths {
        ensure!(
            path.is_file(),
            "{} is not a file; drop files rather than folders",
            path.display()
        );
        let name = path
            .file_name()
            .context("file name")?
            .to_string_lossy()
            .into_owned();
        validate_name(&name)?;
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        total = total.checked_add(size).context("file sizes overflow")?;
        files.push((file, Header { name, size }));
    }
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.write_all(FILE_MAGIC)?;
    stream.write_all(&(files.len() as u32).to_le_bytes())?;
    let mut buffer = vec![0; CHUNK];
    let mut sent = 0u64;
    let start = Instant::now();
    let mut names = vec![];
    for (mut file, header) in files {
        write_packet(&mut stream, &serde_json::to_vec(&header)?)?;
        receive_reply(&mut stream)?;
        let mut digest = Sha256::new();
        let mut remaining = header.size;
        while remaining > 0 {
            ensure!(!cancelled(), "file transfer cancelled");
            let n = remaining.min(CHUNK as u64) as usize;
            file.read_exact(&mut buffer[..n])
                .context("file changed during transfer")?;
            digest.update(&buffer[..n]);
            stream.write_all(&buffer[..n])?;
            remaining -= n as u64;
            sent += n as u64;
            progress(sent, total, &header.name);
            // Leave most of gigabit LAN capacity available for live video.
            let budget = Duration::from_secs_f64(sent as f64 / (15.0 * 1024.0 * 1024.0));
            if let Some(delay) = budget.checked_sub(start.elapsed()) {
                std::thread::sleep(delay.min(Duration::from_millis(10)));
            }
        }
        stream.write_all(&digest.finalize())?;
        names.push(receive_reply(&mut stream)?);
    }
    progress(sent, total, "");
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_paths_and_device_names() {
        for bad in [
            "../x", "a/b", "a\\b", "a:stream", "CON.txt", "nul", "LPT1.bin", "a.", "a ", "",
        ] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        for good in ["report.txt", "żółw 🐢.txt", "COM10.txt"] {
            validate_name(good).unwrap();
        }
    }
    #[test]
    fn transfer_verifies_bytes_and_keeps_existing_file() {
        let root =
            std::env::temp_dir().join(format!("screener-transfer-test-{}", std::process::id()));
        let source = root.join("source");
        let destination = root.join("desktop");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let name = "żółw.txt";
        let data: Vec<u8> = (0..300000).map(|i| (i % 251) as u8).collect();
        fs::write(source.join(name), &data).unwrap();
        fs::write(destination.join(name), b"original").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let dest = destination.clone();
        let worker =
            std::thread::spawn(move || receive(listener.accept().unwrap().0, &dest).unwrap());
        let names = send(address, &[source.join(name)], |_, _, _| {}).unwrap();
        let received = worker.join().unwrap();
        assert_eq!(names, vec!["żółw (2).txt"]);
        assert_eq!(fs::read(&received[0]).unwrap(), data);
        assert_eq!(fs::read(destination.join(name)).unwrap(), b"original");
        // Delete only this test's explicitly created temporary directory.
        assert!(root
            .canonicalize()
            .unwrap()
            .starts_with(std::env::temp_dir().canonicalize().unwrap()));
        assert_eq!(
            root.file_name().unwrap().to_str().unwrap(),
            format!("screener-transfer-test-{}", std::process::id())
        );
        fs::remove_dir_all(root).unwrap();
    }
}
