//! Developer-only stage benchmark; not copied into the portable distribution.
use anyhow::{bail, Result};
use clap::Parser;
use screener::{
    config::ServerConfig,
    latency::{self, Marker},
    media::{self, ManagedChild, Pipeline},
    util::TimerResolution,
};
use std::{
    io::Read,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use windows::Win32::UI::HiDpi::*;

fn main() -> Result<()> {
    let _timer = TimerResolution::new();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _marker = Marker::start()?;
    let stage = std::env::args().nth(1).unwrap_or_else(|| "capture".into());
    let mut c = ServerConfig::parse_from(["server", "--codec", "hevc"]);
    c.capture = screener::config::CaptureMode::Ddagrab;
    if std::env::args().any(|a| a == "--h264") {
        c.codec = screener::config::Codec::H264;
    }
    let filter="hwdownload,format=bgra,vpp_amf=w=1920:h=1080:format=nv12:color_profile=bt709:in_color_range=full:out_color_range=studio:out_trc=bt709:out_primaries=bt709";
    let pipeline = Pipeline {
        encoder: screener::config::Encoder::Amf,
        filter: filter.into(),
        label: "benchmark".into(),
    };
    let (mut consumer, _producer, width, height) = if stage == "capture" || stage == "scale" {
        let mut cmd = Command::new(media::runtime_file("ffmpeg.exe")?);
        cmd.creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let (vf, w, h) = if stage == "capture" {
            (
                "hwdownload,format=bgra,crop=448:24:0:0".to_string(),
                448,
                24,
            )
        } else {
            (
                format!("{filter},hwdownload,format=nv12,crop=336:18:0:0,format=bgra"),
                336,
                18,
            )
        };
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-filter_threads",
            "1",
            "-f",
            "lavfi",
            "-i",
            "ddagrab=framerate=60:draw_mouse=0",
            "-vf",
            &vf,
            "-an",
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-flush_packets",
            "1",
            "pipe:1",
        ]);
        (ManagedChild::spawn(&mut cmd)?, None, w, h)
    } else if stage == "codec" || stage == "hwdecode" || stage == "tcp" {
        let mut cmd = media::ffmpeg_command(&c, &pipeline)?;
        if std::env::args().any(|a| a == "--low-delay") {
            cmd.args(["-flags", "+low_delay", "-latency", "1"]);
        }
        cmd.args([
            "-f",
            if stage == "tcp" { "nut" } else { "mpegts" },
            "-mpegts_flags",
            "+resend_headers",
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            "-flush_packets",
            "1",
            "pipe:1",
        ])
        .stderr(Stdio::null());
        if stage == "tcp" {
            cmd = Command::new(std::env::current_exe()?.with_file_name("server.exe"));
            cmd.creation_flags(0x08000000)
                .args([
                    "--bind",
                    "127.0.0.1",
                    "--port",
                    "48090",
                    "--input-port",
                    "48091",
                    "--smoke-seconds",
                    "15",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        }
        let mut producer = ManagedChild::spawn(&mut cmd)?;
        let input = if stage == "tcp" {
            let deadline = Instant::now() + Duration::from_secs(5);
            while std::net::TcpStream::connect("127.0.0.1:48091").is_err() {
                if Instant::now() > deadline {
                    bail!("server did not start");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Stdio::null()
        } else {
            Stdio::from(producer.child.stdout.take().unwrap())
        };
        let mut cmd = Command::new(media::runtime_file("ffmpeg.exe")?);
        cmd.creation_flags(0x08000000)
            .stdin(input)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-threads",
            "1",
            "-fflags",
            "+nobuffer",
            "-flags",
            "+low_delay",
            "-probesize",
            "32",
            "-analyzeduration",
            "1",
            "-f",
            if stage == "tcp" { "nut" } else { "mpegts" },
        ]);
        let vf = if stage == "hwdecode" || stage == "tcp" {
            cmd.args(["-hwaccel", "d3d11va", "-hwaccel_output_format", "d3d11"]);
            "hwdownload,format=nv12,crop=336:18:0:0,format=bgra"
        } else {
            "crop=336:18:0:0,format=bgra"
        };
        cmd.args([
            "-i",
            if stage == "tcp" {
                "tcp://127.0.0.1:48090?tcp_nodelay=1"
            } else {
                "pipe:0"
            },
            "-an",
            "-vf",
            vf,
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-flush_packets",
            "1",
            "pipe:1",
        ]);
        (ManagedChild::spawn(&mut cmd)?, Some(producer), 336, 18)
    } else {
        bail!("stage must be capture, scale, codec, hwdecode or tcp")
    };
    let mut pipe = consumer.child.stdout.take().unwrap();
    let mut pixels = vec![0; width * height * 4];
    let mut ages = vec![];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(6) {
        pipe.read_exact(&mut pixels)?;
        if start.elapsed() > Duration::from_secs(2) {
            if let Ok(stamp) = latency::read_marker(&pixels, width, height, width * 4, 448, 24) {
                let age = latency::tick().wrapping_sub(stamp);
                if age < 60000 {
                    ages.push(age);
                }
            }
        }
    }
    ages.sort_unstable();
    if ages.is_empty() {
        bail!("no valid source-clock samples");
    }
    println!(
        "STAGE {stage} {:?}: n={} min={} median={} p95={} max={} ms",
        c.codec,
        ages.len(),
        ages[0],
        ages[ages.len() / 2],
        ages[ages.len() * 95 / 100],
        ages[ages.len() - 1]
    );
    Ok(())
}
