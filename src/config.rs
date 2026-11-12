use clap::{Parser, ValueEnum};

pub const VIDEO_PORT: u16 = 47990;
pub const INPUT_PORT: u16 = 47991;
pub const DISCOVERY_PORT: u16 = 47992;
pub const FILE_PORT: u16 = 47993;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum RateControl {
    Quality,
    Cbr,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Preset {
    Speed,
    Balanced,
    Quality,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum CaptureMode {
    Native,
    Ddagrab,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Encoder {
    Auto,
    Amf,
    Nvenc,
    Qsv,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Hardware accelerated LAN desktop server. Starts listening immediately."
)]
pub struct ServerConfig {
    #[arg(long, default_value = "0.0.0.0")]
    pub bind: String,
    #[arg(long, default_value_t = VIDEO_PORT)]
    pub port: u16,
    #[arg(long, default_value_t = INPUT_PORT)]
    pub input_port: u16,
    #[arg(long,default_value_t=FILE_PORT)]
    pub file_port: u16,
    /// Constant stream cadence, including duplicate images while the desktop is idle.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u16).range(1..=240))]
    pub fps: u16,
    #[arg(long, default_value_t = 1920, value_parser = clap::value_parser!(u16).range(320..=7680))]
    pub width: u16,
    #[arg(long, default_value_t = 1080, value_parser = clap::value_parser!(u16).range(200..=4320))]
    pub height: u16,
    /// Output index on the default DXGI adapter.
    #[arg(long, default_value_t = 0)]
    pub monitor: u32,
    #[arg(long, value_enum, default_value_t = Encoder::Auto)]
    pub encoder: Encoder,
    /// Hardware video codec; HEVC is the default for quality per bit.
    #[arg(long, value_enum, default_value_t = Codec::Hevc)]
    pub codec: Codec,
    /// Fixed quantization preserves quality; CBR enforces the bitrate target instead.
    #[arg(long,value_enum,default_value_t=RateControl::Quality)]
    pub rate_control: RateControl,
    #[arg(long,default_value_t=16,value_parser=clap::value_parser!(u8).range(0..=51))]
    pub qp: u8,
    #[arg(long,value_enum,default_value_t=Preset::Speed)]
    pub preset: Preset,
    #[arg(long,value_enum,default_value_t=CaptureMode::Native)]
    pub capture: CaptureMode,
    /// Video target bitrate in megabits per second when --rate-control cbr is selected.
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u32).range(5..=800))]
    pub mbps: u32,
    /// Capture and encode a five-second local hardware benchmark, then exit.
    #[arg(long)]
    pub probe: bool,
    /// Do not capture desktop audio.
    #[arg(long)]
    pub no_audio: bool,
    /// Do not add Windows Firewall rules when installing the service.
    #[arg(long)]
    pub no_firewall: bool,
    #[arg(long)]
    pub smoke_seconds: Option<u64>,
    /// Internal: run the interactive session worker directly.
    #[arg(long, hide = true)]
    pub worker: bool,
    /// Internal: service control manager entry point.
    #[arg(long, hide = true)]
    pub service: bool,
    /// Internal: install or update the background service, then exit.
    #[arg(long, hide = true)]
    pub install_service: bool,
    /// Internal: stop and remove the background service, then exit.
    #[arg(long, hide = true)]
    pub uninstall_service: bool,
    /// Internal: report background service state and exit.
    #[arg(long, hide = true)]
    pub service_status: bool,
    /// Internal: Windows session hosting the worker.
    #[arg(long, hide = true)]
    pub session: Option<u32>,
    /// Internal: signed-in user's Desktop for received files.
    #[arg(long, hide = true)]
    pub desktop: Option<std::path::PathBuf>,
}

#[derive(Parser, Debug, Clone)]
#[command(
    version,
    about = "Direct3D LAN viewer. Discovers and connects automatically."
)]
pub struct ViewerConfig {
    /// Optional server IP or hostname. Without it, use LAN broadcast discovery.
    pub server: Option<String>,
    #[arg(long, default_value_t = VIDEO_PORT)]
    pub port: u16,
    #[arg(long, default_value_t = INPUT_PORT)]
    pub input_port: u16,
    #[arg(long)]
    pub fullscreen: bool,
    /// Start with raw relative mouse input for games.
    #[arg(long)]
    pub relative: bool,
    /// Connect and display without forwarding keyboard or mouse input.
    #[arg(long)]
    pub view_only: bool,
    /// Close automatically after this many seconds (diagnostics).
    #[arg(long)]
    pub smoke_seconds: Option<u64>,
    /// Loopback diagnostic: show a timestamp barcode and measure decoded frame age.
    #[arg(long, requires = "view_only")]
    pub latency_probe: bool,
    /// Do not play desktop audio.
    #[arg(long)]
    pub no_audio: bool,
    /// Audio playout buffer in milliseconds; trades latency for loss tolerance.
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(10..=500))]
    pub audio_buffer_ms: u16,
    /// Diagnostic: check rendering while a nested Windows message loop runs.
    #[arg(long, hide = true, requires = "view_only")]
    pub modal_probe: bool,
    /// Diagnostic: discard every Nth video datagram before loss recovery.
    #[arg(long, hide = true, requires = "view_only")]
    pub video_drop_every: Option<u32>,
    /// Diagnostic comparison with the original player buffering settings.
    #[arg(long, hide = true)]
    pub legacy_buffering: bool,
    #[arg(long,default_value_t=FILE_PORT)]
    pub file_port: u16,
    /// Send files to the connected server's Desktop and exit (also supports Explorer drops).
    #[arg(long,num_args=1..)]
    pub send: Vec<std::path::PathBuf>,
}
