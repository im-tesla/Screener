use crate::{
    config::DISCOVERY_PORT,
    util::{log, private_peer},
};
use anyhow::Result;
use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const QUERY: &[u8] = b"SCREENER_DISCOVER_4";
const RESPONSE: &[u8] = b"SCREENER_HERE_4";

pub fn serve(bind: &str, video: u16, input: u16, alive: Arc<AtomicBool>) -> Result<()> {
    let sock = UdpSocket::bind((bind, DISCOVERY_PORT))?;
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;
    std::thread::spawn(move || {
        let mut buf = [0; 256];
        let mut response = RESPONSE.to_vec();
        response.extend_from_slice(&video.to_le_bytes());
        response.extend_from_slice(&input.to_le_bytes());
        while alive.load(Ordering::Relaxed) {
            if let Ok((n, peer)) = sock.recv_from(&mut buf) {
                if &buf[..n] == QUERY && private_peer(peer.ip()) {
                    let _ = sock.send_to(&response, peer);
                }
            }
        }
    });
    Ok(())
}

pub fn discover(timeout: Duration) -> Result<Option<(SocketAddr, u16)>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_broadcast(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let deadline = Instant::now() + timeout;
    let mut buf = [0; 256];
    let mut next = Instant::now();
    while Instant::now() < deadline {
        if Instant::now() >= next {
            let _ = sock.send_to(QUERY, (Ipv4Addr::BROADCAST, DISCOVERY_PORT));
            // Also permits a local end-to-end diagnostic without a LAN adapter.
            let _ = sock.send_to(QUERY, (Ipv4Addr::LOCALHOST, DISCOVERY_PORT));
            next = Instant::now() + Duration::from_millis(500);
        }
        if let Ok((n, peer)) = sock.recv_from(&mut buf) {
            if n == RESPONSE.len() + 4
                && &buf[..RESPONSE.len()] == RESPONSE
                && private_peer(peer.ip())
            {
                let video = u16::from_le_bytes(buf[n - 4..n - 2].try_into()?);
                let input = u16::from_le_bytes(buf[n - 2..n].try_into()?);
                log(format!("Discovered server {}:{video}", peer.ip()));
                return Ok(Some((SocketAddr::new(peer.ip(), video), input)));
            }
        }
    }
    Ok(None)
}
