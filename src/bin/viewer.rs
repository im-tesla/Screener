#![windows_subsystem = "windows"]
use clap::Parser;
fn main() {
    screener::util::init_log("viewer");
    let _timer = screener::util::TimerResolution::new();
    let config = screener::config::ViewerConfig::parse();
    let result = if config.legacy_buffering {
        screener::util::prepare_runtime(
            "libmpv-2.dll",
            include_bytes!(concat!(env!("OUT_DIR"), "/libmpv-2.dll.zst")),
            env!("SCREENER_MPV_SHA"),
            env!("SCREENER_MPV_SIZE"),
        )
    } else {
        screener::util::prepare_runtime(
            "ffmpeg.exe",
            include_bytes!(concat!(env!("OUT_DIR"), "/ffmpeg.exe.zst")),
            env!("SCREENER_FFMPEG_SHA"),
            env!("SCREENER_FFMPEG_SIZE"),
        )
    };
    let result = result.and_then(|_| screener::viewer::run(config));
    if let Err(e) = result {
        screener::util::log(format!("FATAL: {e:#}"));
        std::process::exit(1);
    }
}
