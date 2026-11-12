//! The Rust applications own networking, discovery, lifecycle, windows, and input.
//! FFmpeg/AMF/NVENC/QSV and libmpv provide the mature native video engines.
use crate::{
    config::{CaptureMode, Codec, Encoder, Preset, RateControl, ServerConfig},
    util::log,
};
use anyhow::{bail, ensure, Context, Result};
use libloading::Library;
use std::{
    ffi::{c_char, c_int, c_void, CStr, CString},
    fs::OpenOptions,
    os::windows::{io::AsRawHandle, process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::JobObjects::*,
};

pub fn runtime_file(name: &str) -> Result<PathBuf> {
    if let Some(path) = crate::util::bundled_runtime() {
        if path.file_name().is_some_and(|n| n == name) {
            return Ok(path.to_path_buf());
        }
    }
    let exe = std::env::current_exe()?;
    for p in [
        exe.with_file_name("runtime").join(name),
        exe.with_file_name(name),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("vendor")
            .join(name),
    ] {
        if p.is_file() {
            return Ok(p);
        }
    }
    bail!("Missing runtime/{name}. Keep the runtime directory beside the executable.")
}

#[derive(Clone, Debug)]
pub struct Pipeline {
    pub encoder: Encoder,
    pub filter: String,
    pub label: String,
}

pub fn ffmpeg_command(c: &ServerConfig, p: &Pipeline) -> Result<Command> {
    let mut cmd = Command::new(runtime_file("ffmpeg.exe")?);
    let codec = match (c.codec, p.encoder) {
        (Codec::Hevc, Encoder::Amf) => "hevc_amf",
        (Codec::H264, Encoder::Amf) => "h264_amf",
        (Codec::Hevc, Encoder::Nvenc) => "hevc_nvenc",
        (Codec::H264, Encoder::Nvenc) => "h264_nvenc",
        (Codec::Hevc, Encoder::Qsv) => "hevc_qsv",
        (Codec::H264, Encoder::Qsv) => "h264_qsv",
        _ => bail!("unresolved hardware encoder"),
    };
    cmd.creation_flags(0x08000000).stdout(Stdio::piped());
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "info",
        "-nostats",
        "-filter_threads",
        "1",
        "-thread_queue_size",
        "1",
    ]);
    if c.capture == CaptureMode::Native {
        let (_, rect) = crate::server::display_info_quiet(c.monitor)?;
        cmd.stdin(Stdio::piped()).args([
            "-f",
            "rawvideo",
            "-pixel_format",
            "bgra",
            "-video_size",
            &format!("{}x{}", rect.width, rect.height),
            "-framerate",
            &c.fps.to_string(),
            "-i",
            "pipe:0",
        ]);
    } else {
        cmd.stdin(Stdio::null()).args([
            "-f",
            "lavfi",
            "-i",
            &format!(
                "ddagrab=output_idx={}:framerate={}:draw_mouse=0:dup_frames=1",
                c.monitor, c.fps
            ),
        ]);
    }
    let filter = if c.capture == CaptureMode::Native {
        p.filter.replace("hwdownload,format=bgra,", "")
    } else {
        p.filter.clone()
    };
    cmd.args([
        "-vf",
        &format!("{filter},setsar=1,setpts=N/({}*TB)", c.fps),
        "-an",
    ]);
    match p.encoder {
        Encoder::Amf => {
            cmd.args([
                "-c:v",
                codec,
                "-usage",
                "ultralowlatency",
                "-quality",
                match c.preset {
                    Preset::Speed => "speed",
                    Preset::Balanced => "balanced",
                    Preset::Quality => "quality",
                },
                "-async_depth",
                "1",
                "-preanalysis",
                "0",
                "-preencode",
                "0",
                "-latency",
                "1",
            ]);
            if c.codec == Codec::Hevc {
                cmd.args(["-header_insertion_mode", "idr", "-gops_per_idr", "1"]);
            }
        }
        Encoder::Nvenc => {
            cmd.args([
                "-c:v",
                codec,
                "-preset",
                match c.preset {
                    Preset::Speed => "p1",
                    Preset::Balanced => "p3",
                    Preset::Quality => "p4",
                },
                "-tune",
                "ull",
                "-rc-lookahead",
                "0",
                "-zerolatency",
                "1",
                "-delay",
                "0",
                "-surfaces",
                "4",
            ]);
        }
        Encoder::Qsv => {
            cmd.args([
                "-c:v",
                codec,
                "-preset",
                "veryfast",
                "-async_depth",
                "1",
                "-look_ahead",
                "0",
            ]);
        }
        Encoder::Auto => bail!("unresolved encoder"),
    }
    if c.rate_control == RateControl::Quality {
        let qp = c.qp.to_string();
        match p.encoder {
            Encoder::Amf => {
                cmd.args(["-rc", "cqp", "-qp_i", &qp, "-qp_p", &qp, "-vbaq", "0"]);
            }
            Encoder::Nvenc => {
                cmd.args(["-rc", "constqp", "-qp", &qp]);
            }
            Encoder::Qsv => {
                cmd.args(["-global_quality", &qp]);
            }
            Encoder::Auto => unreachable!(),
        }
    } else {
        let bitrate = format!("{}M", c.mbps);
        // Four frames absorb normal IDR/desktop bursts; the buffer is a rate-control
        // budget, not a playback queue. The old single-frame budget crushed detail.
        let vbv = (c.mbps as u64 * 1_000_000 * 4 / c.fps as u64).to_string();
        cmd.args([
            "-rc", "cbr", "-b:v", &bitrate, "-maxrate", &bitrate, "-bufsize", &vbv,
        ]);
    }
    cmd.args([
        "-g",
        &c.fps.to_string(),
        "-bf",
        "0",
        "-flags",
        "+low_delay",
        "-profile:v",
        if c.codec == Codec::Hevc {
            "main"
        } else {
            "high"
        },
        "-fps_mode",
        "passthrough",
        "-enc_time_base",
        &format!("1:{}", c.fps),
        "-color_range",
        "tv",
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
    ]);
    Ok(cmd)
}

pub fn choose_pipeline(c: &ServerConfig, detected: Encoder) -> Result<Pipeline> {
    let enc = if matches!(c.encoder, Encoder::Auto) {
        detected
    } else {
        c.encoder
    };
    let size = format!("w={}:h={}", c.width, c.height);
    let mut filters: Vec<(&str,String)> = match enc {
        Encoder::Amf => vec![
            ("Direct3D capture -> GPU AMF scale/encode", format!("vpp_amf={size}:format=nv12:color_profile=bt709:in_color_range=full:out_color_range=studio:out_trc=bt709:out_primaries=bt709")),
            ("Direct3D capture -> readback -> GPU AMF scale/encode", format!("hwdownload,format=bgra,vpp_amf={size}:format=nv12:color_profile=bt709:in_color_range=full:out_color_range=studio:out_trc=bt709:out_primaries=bt709")),
            ("Direct3D capture -> CPU scale -> GPU AMF encode", format!("hwdownload,format=bgra,scale={}:{}:flags=bilinear:out_color_matrix=bt709:in_range=pc:out_range=tv,format=nv12",c.width,c.height)),
        ],
        Encoder::Nvenc => vec![
            ("Direct3D capture -> GPU scale -> NVENC", format!("scale_d3d11=width={}:height={}:format=nv12",c.width,c.height)),
            ("Direct3D capture -> CPU scale -> NVENC", format!("hwdownload,format=bgra,scale={}:{}:flags=bilinear:out_color_matrix=bt709:in_range=pc:out_range=tv,format=nv12",c.width,c.height)),
        ],
        Encoder::Qsv => vec![("Direct3D capture -> CPU scale -> GPU QSV encode",format!("hwdownload,format=bgra,scale={}:{}:flags=bilinear:out_color_matrix=bt709:in_range=pc:out_range=tv,format=nv12",c.width,c.height))],
        Encoder::Auto => bail!("No supported hardware encoder detected; use --encoder amf, nvenc, or qsv"),
    };
    // Avoid AMF's additional converter/context when the monitor already matches
    // the requested resolution. In particular, RX 580 hosts do not need scaling.
    if let Ok((_, rect)) = crate::server::display_info_quiet(c.monitor) {
        if rect.width == c.width as i32 && rect.height == c.height as i32 {
            filters.insert(0,("Direct3D capture -> fast CPU NV12 conversion (no resize) -> hardware encode",
                "hwdownload,format=bgra,scale=iw:ih:flags=fast_bilinear:out_color_matrix=bt709:in_range=pc:out_range=tv,format=nv12".into()));
        }
    }
    for (label, filter) in filters {
        let p = Pipeline {
            encoder: enc,
            filter,
            label: label.into(),
        };
        let mut cmd = ffmpeg_command(c, &p)?;
        cmd.args(["-frames:v", "3", "-f", "null", "-"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let start = std::time::Instant::now();
        let out = if c.capture == CaptureMode::Native {
            let (_, rect) = crate::server::display_info_quiet(c.monitor)?;
            let mut process = cmd.spawn()?;
            let mut input = process.stdin.take().context("probe input")?;
            let sender = std::thread::spawn(move || {
                let frame = vec![96u8; rect.width as usize * rect.height as usize * 4];
                for _ in 0..3 {
                    use std::io::Write;
                    if input.write_all(&frame).is_err() {
                        break;
                    }
                }
            });
            let result = process.wait_with_output()?;
            let _ = sender.join();
            result
        } else {
            cmd.output().context("starting hardware encoder probe")?
        };
        if out.status.success() {
            log(format!(
                "Selected {} ({:?} probe)",
                p.label,
                start.elapsed()
            ));
            return Ok(p);
        }
        log(format!(
            "Pipeline unavailable: {label}\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    bail!("Hardware encoding failed. See server.log for driver/encoder diagnostics.")
}

pub fn encoder_log() -> Result<std::fs::File> {
    Ok(OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::current_exe()?.with_file_name("encoder.log"))?)
}

pub struct ManagedChild {
    pub child: Child,
    job: HANDLE,
}
impl ManagedChild {
    pub fn spawn(cmd: &mut Command) -> Result<Self> {
        let mut child = cmd.spawn()?;
        let result = unsafe {
            (|| -> Result<HANDLE> {
                let job = CreateJobObjectW(None, None)?;
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if let Err(e) = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const c_void,
                    std::mem::size_of_val(&limits) as u32,
                ) {
                    let _ = CloseHandle(job);
                    return Err(e.into());
                }
                if let Err(e) = AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) {
                    let _ = CloseHandle(job);
                    return Err(e.into());
                }
                Ok(job)
            })()
        };
        match result {
            Ok(job) => Ok(Self { child, job }),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }
}
impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        unsafe {
            let _ = CloseHandle(self.job);
        }
    }
}

type Create = unsafe extern "C" fn() -> *mut c_void;
type Destroy = unsafe extern "C" fn(*mut c_void);
type SetOption = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type Initialize = unsafe extern "C" fn(*mut c_void) -> c_int;
type CommandFn = unsafe extern "C" fn(*mut c_void, *const *const c_char) -> c_int;
type Wait = unsafe extern "C" fn(*mut c_void, f64) -> *const MpvEvent;
type GetString = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char;
type Free = unsafe extern "C" fn(*mut c_void);
type ErrorString = unsafe extern "C" fn(c_int) -> *const c_char;
type CommandRet = unsafe extern "C" fn(*mut c_void, *const *const c_char, *mut Node) -> c_int;
type FreeNode = unsafe extern "C" fn(*mut Node);
#[repr(C)]
union NodeValue {
    int: i64,
    ptr: *mut c_void,
}
#[repr(C)]
struct Node {
    value: NodeValue,
    format: c_int,
}
#[repr(C)]
struct NodeList {
    num: c_int,
    values: *mut Node,
    keys: *mut *mut c_char,
}
#[repr(C)]
struct ByteArray {
    data: *mut u8,
    size: usize,
}
impl Node {
    unsafe fn entry(&self, key: &str) -> Result<&Node> {
        ensure!(
            self.format == 8 && !self.value.ptr.is_null(),
            "screenshot result is not a map"
        );
        let list = &*(self.value.ptr as *const NodeList);
        ensure!((0..=32).contains(&list.num), "invalid screenshot map");
        for i in 0..list.num as usize {
            if CStr::from_ptr(*list.keys.add(i)).to_bytes() == key.as_bytes() {
                return Ok(&*list.values.add(i));
            }
        }
        bail!("screenshot missing {key}")
    }
    unsafe fn integer(&self) -> Result<usize> {
        ensure!(
            self.format == 4 && self.value.int >= 0,
            "invalid screenshot integer"
        );
        Ok(self.value.int as usize)
    }
}

#[repr(C)]
pub struct MpvEvent {
    pub id: c_int,
    pub error: c_int,
    pub userdata: u64,
    pub data: *mut c_void,
}

pub struct MpvPlayer {
    handle: *mut c_void,
    destroy: Destroy,
    set: SetOption,
    command: CommandFn,
    wait: Wait,
    get: GetString,
    free: Free,
    error: ErrorString,
    command_ret: CommandRet,
    free_node: FreeNode,
    _lib: Library,
}
impl MpvPlayer {
    pub fn new(hwnd: isize, legacy_buffering: bool) -> Result<Self> {
        unsafe {
            let lib = Library::new(runtime_file("libmpv-2.dll")?).context("loading libmpv")?;
            let create: Create = *lib.get(b"mpv_create\0")?;
            let init: Initialize = *lib.get(b"mpv_initialize\0")?;
            let p = Self {
                handle: create(),
                destroy: *lib.get(b"mpv_terminate_destroy\0")?,
                set: *lib.get(b"mpv_set_option_string\0")?,
                command: *lib.get(b"mpv_command\0")?,
                wait: *lib.get(b"mpv_wait_event\0")?,
                get: *lib.get(b"mpv_get_property_string\0")?,
                free: *lib.get(b"mpv_free\0")?,
                error: *lib.get(b"mpv_error_string\0")?,
                command_ret: *lib.get(b"mpv_command_ret\0")?,
                free_node: *lib.get(b"mpv_free_node_contents\0")?,
                _lib: lib,
            };
            ensure!(!p.handle.is_null(), "mpv_create failed");
            p.option("wid", &hwnd.to_string())?;
            let logpath = std::env::current_exe()?.with_file_name("decoder.log");
            p.option("log-file", &logpath.to_string_lossy())?;
            for (k, v) in [
                ("config", "no"),
                ("load-scripts", "no"),
                ("profile", "low-latency"),
                ("vo", "gpu"),
                ("gpu-api", "d3d11"),
                ("gpu-context", "d3d11"),
                ("hwdec", "d3d11va"),
                ("hwdec-codecs", "all"),
                ("vd-lavc-software-fallback", "no"),
                ("d3d11-sync-interval", "0"),
                ("swapchain-depth", "1"),
                ("audio", "no"),
                ("osc", "no"),
                ("osd-level", "0"),
                ("input-default-bindings", "no"),
                ("input-vo-keyboard", "no"),
                ("input-cursor", "no"),
                ("cursor-autohide", "no"),
                ("keep-open", "no"),
                ("idle", "yes"),
                ("force-window", "yes"),
                ("cache", "no"),
                ("cache-pause", "no"),
                ("demuxer-readahead-secs", "0"),
                ("demuxer-max-bytes", "1MiB"),
                ("demuxer-max-back-bytes", "0"),
                ("demuxer-lavf-format", "mpegts"),
                ("demuxer-lavf-probesize", "32768"),
                ("demuxer-lavf-analyzeduration", "0"),
                ("demuxer-lavf-o", "fflags=+nobuffer"),
                ("untimed", "yes"),
                ("video-sync", "desync"),
                ("interpolation", "no"),
                ("msg-level", "all=info"),
                ("ytdl", "no"),
                ("scripts", ""),
                ("vd-lavc-threads", "1"),
                ("video-latency-hacks", "yes"),
            ] {
                p.option(k, v)?;
            }
            if !legacy_buffering {
                for (key, value) in [
                    ("stream-buffer-size", "4KiB"),
                    ("demuxer-lavf-buffersize", "512"),
                    ("demuxer-lavf-probesize", "32"),
                    ("demuxer-lavf-probe-info", "nostreams"),
                    ("demuxer-max-bytes", "64KiB"),
                    ("demuxer-lavf-o", "fflags=+nobuffer,avioflags=direct"),
                    ("vd-queue-enable", "no"),
                    ("hwdec-extra-frames", "2"),
                    ("vd-lavc-o", "flags=+low_delay"),
                    ("stream-lavf-o", "avioflags=direct"),
                    ("correct-pts", "no"),
                    ("container-fps-override", "60"),
                ] {
                    p.option(key, value)?;
                }
            }
            p.check(init(p.handle))?;
            Ok(p)
        }
    }
    fn check(&self, status: c_int) -> Result<()> {
        if status < 0 {
            unsafe {
                bail!(
                    "mpv: {}",
                    CStr::from_ptr((self.error)(status)).to_string_lossy()
                );
            }
        }
        Ok(())
    }
    pub fn option(&self, k: &str, v: &str) -> Result<()> {
        let k = CString::new(k)?;
        let v = CString::new(v)?;
        self.check(unsafe { (self.set)(self.handle, k.as_ptr(), v.as_ptr()) })
            .with_context(|| format!("setting {}", k.to_string_lossy()))
    }
    pub fn load(&self, url: &str) -> Result<()> {
        self.command(&["loadfile", url, "replace"])
    }
    pub fn stop(&self) {
        let _ = self.command(&["stop"]);
    }
    pub fn command(&self, args: &[&str]) -> Result<()> {
        let strings: Vec<CString> = args
            .iter()
            .map(|s| CString::new(*s))
            .collect::<std::result::Result<_, _>>()?;
        let mut ptrs: Vec<_> = strings.iter().map(|s| s.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        self.check(unsafe { (self.command)(self.handle, ptrs.as_ptr()) })
    }
    pub fn property(&self, name: &str) -> Option<String> {
        let name = CString::new(name).ok()?;
        unsafe {
            let s = (self.get)(self.handle, name.as_ptr());
            if s.is_null() {
                return None;
            }
            let out = CStr::from_ptr(s).to_string_lossy().into_owned();
            (self.free)(s.cast());
            Some(out)
        }
    }
    pub fn poll(&self) -> Option<(i32, i32)> {
        unsafe {
            let e = (self.wait)(self.handle, 0.0);
            if e.is_null() || (*e).id == 0 {
                None
            } else {
                let error = if (*e).id == 7 && !(*e).data.is_null() {
                    *(((*e).data as *const i32).add(1))
                } else {
                    (*e).error
                };
                Some(((*e).id, error))
            }
        }
    }
    pub fn frame_age(&self, source_w: usize, source_h: usize) -> Result<(u32, u32)> {
        let before = crate::latency::tick();
        unsafe {
            let mut node = Node {
                value: NodeValue { int: 0 },
                format: 0,
            };
            let args = [
                c"screenshot-raw".as_ptr(),
                c"video".as_ptr(),
                c"bgr0".as_ptr(),
                std::ptr::null(),
            ];
            self.check((self.command_ret)(self.handle, args.as_ptr(), &mut node))?;
            let result = (|| -> Result<(u32, u32)> {
                let width = node.entry("w")?.integer()?;
                let height = node.entry("h")?.integer()?;
                let stride = node.entry("stride")?.integer()?;
                let data = node.entry("data")?;
                ensure!(
                    data.format == 9 && !data.value.ptr.is_null(),
                    "screenshot data missing"
                );
                let bytes = &*(data.value.ptr as *const ByteArray);
                ensure!(
                    !bytes.data.is_null() && bytes.size <= 512 * 1024 * 1024,
                    "invalid screenshot data"
                );
                let pixels = std::slice::from_raw_parts(bytes.data, bytes.size);
                let stamp =
                    crate::latency::read_marker(pixels, width, height, stride, source_w, source_h)?;
                let now = crate::latency::tick();
                let age = now.wrapping_sub(stamp);
                ensure!(age < 60000, "clock not from this computer");
                Ok((age, now.wrapping_sub(before)))
            })();
            (self.free_node)(&mut node);
            result
        }
    }
}
impl Drop for MpvPlayer {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                (self.destroy)(self.handle);
            }
        }
    }
}
