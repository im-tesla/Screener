#![windows_subsystem = "windows"]
use clap::Parser;
fn main() {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    screener::util::init_log("server");
    let _timer = screener::util::TimerResolution::new();
    let config = screener::config::ServerConfig::parse();
    let result = screener::util::prepare_runtime(
        "ffmpeg.exe",
        include_bytes!(concat!(env!("OUT_DIR"), "/ffmpeg.exe.zst")),
        env!("SCREENER_FFMPEG_SHA"),
        env!("SCREENER_FFMPEG_SIZE"),
    )
    .and_then(|_| screener::server::run(config));
    if let Err(e) = result {
        screener::util::log(format!("FATAL: {e:#}"));
        std::process::exit(1);
    }
}
