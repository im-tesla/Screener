#![windows_subsystem = "windows"]
use anyhow::Result;
use clap::Parser;
use screener::config::ServerConfig;

fn main() {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // The service manager launches this executable with --service.
    if raw.iter().any(|arg| arg == "--service") {
        screener::util::init_log("service");
        if let Err(e) = screener::service::run_service() {
            screener::util::log(format!("FATAL: {e:#}"));
            std::process::exit(1);
        }
        return;
    }
    screener::util::init_log("server");
    let _timer = screener::util::TimerResolution::new();
    let config = ServerConfig::parse();
    if let Err(e) = run(config, &raw) {
        screener::util::log(format!("FATAL: {e:#}"));
        std::process::exit(1);
    }
}

fn run(config: ServerConfig, raw: &[String]) -> Result<()> {
    if config.install_service {
        let args = screener::service::server_args(raw);
        if !screener::service::is_elevated() {
            let mut elevated = vec!["--install-service".to_string()];
            elevated.extend(args);
            return screener::service::request_elevation(&elevated);
        }
        return screener::service::install(&args, !config.no_firewall);
    }
    if config.uninstall_service {
        if !screener::service::is_elevated() {
            return screener::service::request_elevation(&["--uninstall-service".to_string()]);
        }
        return screener::service::uninstall();
    }
    if config.service_status {
        return screener::service::report_status();
    }
    if config.worker {
        screener::service::describe_worker(config.session);
        if let Some(desktop) = &config.desktop {
            screener::files::set_desktop(desktop.clone());
        }
        if config.session.is_some() {
            // The protected service directory also holds the extracted runtime.
            let exe = std::env::current_exe()?;
            if let Some(directory) = exe.parent() {
                screener::util::set_runtime_root(directory.join("runtime"));
            }
        }
        return run_server(config);
    }
    if config.probe || config.smoke_seconds.is_some() {
        // Diagnostics keep running in this process and exit on their own.
        return run_server(config);
    }
    screener::service::ensure(&screener::service::server_args(raw))
}

fn run_server(config: ServerConfig) -> Result<()> {
    screener::util::prepare_runtime(
        "ffmpeg.exe",
        include_bytes!(concat!(env!("OUT_DIR"), "/ffmpeg.exe.zst")),
        env!("SCREENER_FFMPEG_SHA"),
        env!("SCREENER_FFMPEG_SIZE"),
    )?;
    screener::server::run(config)
}
