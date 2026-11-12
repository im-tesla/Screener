use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub build: String,
    pub machine: String,
    pub adapter: String,
    pub encoder: String,
    pub codec: String,
    pub pipeline: String,
    pub rate_control: String,
    pub qp: u8,
    pub file_port: u16,
    pub target_fps: u16,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServerStats {
    #[serde(default)]
    pub captured: u64,
    #[serde(default)]
    pub submitted: u64,
    pub encoded: u64,
    pub bytes: u64,
    pub send_wait_us: u64,
    pub elapsed_ms: u64,
}
#[derive(Default)]
pub struct StreamCounters {
    pub captured: AtomicU64,
    pub submitted: AtomicU64,
    pub encoded: AtomicU64,
    pub bytes: AtomicU64,
    pub send_wait_us: AtomicU64,
    pub start_ms: AtomicU64,
}
impl StreamCounters {
    pub fn reset(&self) {
        self.captured.store(0, Ordering::Relaxed);
        self.submitted.store(0, Ordering::Relaxed);
        self.encoded.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
        self.send_wait_us.store(0, Ordering::Relaxed);
        self.start_ms.store(now_ms(), Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> ServerStats {
        ServerStats {
            captured: self.captured.load(Ordering::Relaxed),
            submitted: self.submitted.load(Ordering::Relaxed),
            encoded: self.encoded.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            send_wait_us: self.send_wait_us.load(Ordering::Relaxed),
            elapsed_ms: now_ms().saturating_sub(self.start_ms.load(Ordering::Relaxed)),
        }
    }
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Debug, Default)]
pub struct ViewerStats {
    pub decoded: u64,
    pub presented: u64,
    pub skipped: u64,
    pub render_us: u64,
    pub elapsed_ms: u64,
    pub udp_received: u64,
    pub udp_recovered: u64,
    pub udp_missing: u64,
    pub udp_late: u64,
}
