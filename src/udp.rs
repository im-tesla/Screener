//! Datagram video: MPEG-TS-aligned shards, XOR parity, and a bounded reorder deadline.
//!
//! Files, discovery, and control remain on TCP. Video is sent as small UDP groups
//! so one lost datagram costs milliseconds instead of stalling the stream. A parity
//! datagram repairs a single missing shard, and data that stays missing past a short
//! deadline is skipped so the MPEG-TS demuxer can resynchronize at the next packet.
use anyhow::{ensure, Context, Result};
use std::{
    collections::BTreeMap,
    io::Write,
    net::{SocketAddr, UdpSocket},
    process::ChildStdin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Session control magic. Bumped whenever the datagram layout changes.
pub const HELLO: &[u8; 8] = b"SCRUDP04";
const MAGIC: &[u8; 4] = b"SV4D";
const HEADER: usize = 32;
/// One MPEG-TS packet is 188 bytes; a shard carries six of them.
pub const SHARD: usize = 188 * 6;
/// A group is four shards plus one XOR parity datagram.
pub const BATCH: usize = SHARD * 4;
const ALIGN: usize = 188;
/// Missing shards are abandoned after this long so playback keeps moving.
const DEADLINE: Duration = Duration::from_millis(10);
/// Groups are independent; only this many may be queued ahead of the stream.
const WINDOW: u64 = 512;
const HEARTBEAT: Duration = Duration::from_millis(250);
const SILENCE: Duration = Duration::from_secs(5);
const RECEIVE_BUFFER: usize = 1 << 20;
const SEND_BUFFER: usize = 1 << 20;

pub const START: u8 = 1;
pub const BEAT: u8 = 2;
pub const STOP: u8 = 3;

pub fn request(session: u64, operation: u8) -> [u8; 17] {
    let mut b = [0; 17];
    b[..8].copy_from_slice(HELLO);
    b[8..16].copy_from_slice(&session.to_le_bytes());
    b[16] = operation;
    b
}

pub fn parse_request(b: &[u8]) -> Option<(u64, u8)> {
    (b.len() == 17 && &b[..8] == HELLO && (START..=STOP).contains(&b[16]))
        .then(|| (u64::from_le_bytes(b[8..16].try_into().unwrap()), b[16]))
}

/// Split one aligned block of video into four data datagrams plus one parity
/// datagram. Every payload stays below a 1280-byte IPv6 path MTU.
pub fn packets(session: u64, group: u64, data: &[u8]) -> Result<Vec<Vec<u8>>> {
    ensure!(
        !data.is_empty() && data.len() <= BATCH && data.len().is_multiple_of(ALIGN),
        "invalid video batch"
    );
    let count = data.len().div_ceil(SHARD);
    let mut lengths = [0u16; 4];
    for (i, chunk) in data.chunks(SHARD).enumerate() {
        lengths[i] = chunk.len() as u16;
    }
    let widest = lengths[0] as usize;
    let mut parity = vec![0u8; widest];
    let header = |index: usize| {
        let mut b = vec![0; HEADER];
        b[..4].copy_from_slice(MAGIC);
        b[4..12].copy_from_slice(&session.to_le_bytes());
        b[12..20].copy_from_slice(&group.to_le_bytes());
        b[20] = count as u8;
        b[21] = index as u8;
        for (i, len) in lengths.iter().enumerate() {
            b[22 + i * 2..24 + i * 2].copy_from_slice(&len.to_le_bytes());
        }
        b
    };
    let mut out = Vec::with_capacity(count + 1);
    for (i, chunk) in data.chunks(SHARD).enumerate() {
        for (p, v) in parity.iter_mut().zip(chunk) {
            *p ^= v;
        }
        let mut b = header(i);
        b.extend_from_slice(chunk);
        out.push(b);
    }
    let mut b = header(count);
    b.extend_from_slice(&parity);
    out.push(b);
    Ok(out)
}

/// Server-side packetizer: accepts the raw MPEG-TS byte stream, sends whole
/// groups immediately, and flushes every complete 188-byte TS packet after each
/// write. The sub-packet remainder waits for the next read so no partial TS
/// packet is ever put on the wire.
pub struct Sender {
    socket: UdpSocket,
    peer: SocketAddr,
    session: u64,
    group: u64,
    fill: Vec<u8>,
}

impl Sender {
    pub fn new(socket: UdpSocket, peer: SocketAddr, session: u64) -> Result<Self> {
        socket2::SockRef::from(&socket).set_send_buffer_size(SEND_BUFFER)?;
        Ok(Self {
            socket,
            peer,
            session,
            group: 0,
            fill: Vec::with_capacity(BATCH + SHARD),
        })
    }

    pub fn session(&self) -> u64 {
        self.session
    }

    pub fn groups(&self) -> u64 {
        self.group
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.fill.extend_from_slice(data);
        let mut buf = std::mem::take(&mut self.fill);
        let mut offset = 0;
        let result = (|| -> Result<()> {
            while buf.len() - offset >= BATCH {
                self.emit(&buf[offset..offset + BATCH])?;
                offset += BATCH;
            }
            let aligned = offset + (buf.len() - offset) / ALIGN * ALIGN;
            if aligned > offset {
                self.emit(&buf[offset..aligned])?;
                offset = aligned;
            }
            Ok(())
        })();
        buf.drain(..offset);
        self.fill = buf;
        result
    }

    fn emit(&mut self, block: &[u8]) -> Result<()> {
        for datagram in packets(self.session, self.group, block)? {
            self.socket.send_to(&datagram, self.peer)?;
        }
        self.group += 1;
        Ok(())
    }
}

struct Group {
    count: usize,
    lengths: [usize; 4],
    shards: [Option<Vec<u8>>; 4],
    parity: Option<Vec<u8>>,
    next: usize,
    created: Instant,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub received: u64,
    pub recovered: u64,
    pub lost: u64,
    pub late: u64,
    pub invalid: u64,
    pub timeouts: u64,
    pub errors: u64,
    pub blocks: u64,
}

impl Stats {
    pub fn datagrams(&self) -> u64 {
        self.received + self.recovered
    }
}

pub struct Reorder {
    session: u64,
    next: u64,
    groups: BTreeMap<u64, Group>,
    pub stats: Stats,
}

impl Reorder {
    pub fn new(session: u64) -> Self {
        Self {
            session,
            next: 0,
            groups: BTreeMap::new(),
            stats: Stats::default(),
        }
    }

    pub fn push(&mut self, b: &[u8], now: Instant) -> Result<()> {
        ensure!(
            b.len() >= HEADER && b.len() <= HEADER + SHARD && &b[..4] == MAGIC,
            "invalid video datagram"
        );
        ensure!(
            u64::from_le_bytes(b[4..12].try_into()?) == self.session,
            "stale video session"
        );
        let id = u64::from_le_bytes(b[12..20].try_into()?);
        if id < self.next {
            // Parity commonly arrives after the group was already reassembled
            // from its data shards; only late video data is a real miss.
            if (b[21] as usize) < b[20] as usize {
                self.stats.late += 1;
            }
            return Ok(());
        }
        if id - self.next >= WINDOW {
            // The sender ran far ahead while this thread was blocked (decoder
            // startup, a long pipe write, a scheduler stall). Skip the gap and
            // resume instead of rejecting every following datagram forever.
            self.stats.lost += id - self.next;
            self.next = id;
            self.groups.retain(|&group, _| group >= id);
        }
        let (count, index) = (b[20] as usize, b[21] as usize);
        ensure!(
            (1..=4).contains(&count) && index <= count,
            "invalid shard index"
        );
        let mut lengths = [0usize; 4];
        for (i, len) in lengths.iter_mut().enumerate() {
            *len = u16::from_le_bytes(b[22 + i * 2..24 + i * 2].try_into()?) as usize;
            ensure!(
                if i < count {
                    *len > 0 && *len <= SHARD && len.is_multiple_of(ALIGN)
                } else {
                    *len == 0
                },
                "invalid shard length"
            );
        }
        let size = if index == count {
            *lengths.iter().max().unwrap()
        } else {
            lengths[index]
        };
        ensure!(b.len() == HEADER + size, "truncated video shard");
        let group = self.groups.entry(id).or_insert_with(|| Group {
            count,
            lengths,
            shards: std::array::from_fn(|_| None),
            parity: None,
            next: 0,
            created: now,
        });
        ensure!(
            group.count == count && group.lengths == lengths,
            "inconsistent shard headers"
        );
        if index == count {
            group.parity.get_or_insert_with(|| b[HEADER..].to_vec());
        } else {
            group.shards[index].get_or_insert_with(|| b[HEADER..].to_vec());
        }
        self.stats.received += 1;
        let missing: Vec<usize> = (0..count).filter(|&i| group.shards[i].is_none()).collect();
        if missing.len() == 1 {
            if let Some(parity) = group.parity.as_ref() {
                let mut recovered = parity.clone();
                for shard in group.shards.iter().take(count).flatten() {
                    for (p, v) in recovered.iter_mut().zip(shard) {
                        *p ^= v;
                    }
                }
                let index = missing[0];
                recovered.truncate(group.lengths[index]);
                group.shards[index] = Some(recovered);
                self.stats.recovered += 1;
            }
        }
        Ok(())
    }

    /// Return reassembled byte runs in order. A group waits up to [`DEADLINE`]
    /// for a missing shard; after that its remaining data is emitted so the
    /// demuxer can resynchronize instead of stalling the stream forever.
    pub fn drain(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut out = vec![];
        loop {
            if let Some(group) = self.groups.get_mut(&self.next) {
                while group.next < group.count {
                    if let Some(b) = &group.shards[group.next] {
                        out.push(b.clone());
                        group.next += 1;
                    } else if now.saturating_duration_since(group.created) >= DEADLINE {
                        self.stats.lost += 1;
                        group.next += 1;
                    } else {
                        return out;
                    }
                }
                self.groups.remove(&self.next);
                self.next += 1;
            } else if let Some((&id, group)) = self.groups.first_key_value() {
                if id < self.next {
                    // Stale entry from a window skip; never move backwards.
                    self.groups.remove(&id);
                    continue;
                }
                if now.saturating_duration_since(group.created) < DEADLINE {
                    break;
                }
                self.stats.lost += id.saturating_sub(self.next);
                self.next = id;
            } else {
                break;
            }
        }
        out
    }
}

/// Viewer-side receiver. Owns the decoder's stdin and feeds it reassembled
/// MPEG-TS bytes; heartbeats tell the server the session is still wanted.
pub struct Bridge {
    active: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<Mutex<Stats>>,
}

impl Bridge {
    pub fn start(address: SocketAddr, mut input: ChildStdin, drop_every: u32) -> Result<Self> {
        let socket = UdpSocket::bind(if address.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })?;
        socket.connect(address)?;
        socket2::SockRef::from(&socket).set_recv_buffer_size(RECEIVE_BUFFER)?;
        socket.set_read_timeout(Some(Duration::from_millis(2)))?;
        let session = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64
            ^ (std::process::id() as u64).rotate_left(32);
        socket.send(&request(session, START))?;
        let active = Arc::new(AtomicBool::new(true));
        let running = active.clone();
        let stats = Arc::new(Mutex::new(Stats::default()));
        let shared = stats.clone();
        let thread = std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                let mut reorder = Reorder::new(session);
                let mut buf = [0; HEADER + SHARD + 1];
                let mut heartbeat = Instant::now();
                let mut last_packet = Instant::now();
                let started = Instant::now();
                let mut datagrams = 0u64;
                let mut first_data = false;
                let mut last_error = None;
                while running.load(Ordering::Relaxed) {
                    if heartbeat.elapsed() >= HEARTBEAT {
                        socket.send(&request(session, BEAT))?;
                        heartbeat = Instant::now();
                    }
                    match socket.recv(&mut buf) {
                        Ok(n) => {
                            datagrams += 1;
                            if drop_every == 0 || !datagrams.is_multiple_of(drop_every as u64) {
                                if reorder.push(&buf[..n], Instant::now()).is_ok() {
                                    last_packet = Instant::now();
                                } else {
                                    reorder.stats.invalid += 1;
                                }
                            }
                        }
                        // A closed server port surfaces as ICMP reset on Windows.
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            reorder.stats.timeouts += 1;
                            if last_error.is_none() {
                                last_error = e.raw_os_error();
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionRefused
                            ) =>
                        {
                            reorder.stats.errors += 1;
                        }
                        Err(e) => return Err(e.into()),
                    }
                    let blocks = reorder.drain(Instant::now());
                    if !blocks.is_empty() && !first_data {
                        first_data = true;
                        crate::util::log(format!(
                            "UDP video: first video data {}ms after connect",
                            started.elapsed().as_millis()
                        ));
                    }
                    for block in blocks {
                        reorder.stats.blocks += 1;
                        if let Err(e) = input.write_all(&block) {
                            if running.load(Ordering::Relaxed) {
                                return Err(e).context("feeding UDP video decoder");
                            }
                            return Ok(());
                        }
                    }
                    *shared.lock().unwrap() = reorder.stats;
                    if last_packet.elapsed() > SILENCE {
                        anyhow::bail!("UDP video timed out");
                    }
                }
                crate::util::log(format!(
                    "UDP video: datagrams={} received={} recovered={} missing_shards={} late={} invalid={} timeouts={} recv_errors={} blocks={} last_error={:?} simulated_drop_every={drop_every}",
                    datagrams,
                    reorder.stats.received,
                    reorder.stats.recovered,
                    reorder.stats.lost,
                    reorder.stats.late,
                    reorder.stats.invalid,
                    reorder.stats.timeouts,
                    reorder.stats.errors,
                    reorder.stats.blocks,
                    last_error
                ));
                Ok(())
            })();
            let _ = socket.send(&request(session, STOP));
            if let Err(e) = result {
                if running.load(Ordering::Relaxed) {
                    crate::util::log(format!("UDP receiver ended: {e:#}"));
                }
            }
            running.store(false, Ordering::Relaxed);
        });
        Ok(Self {
            active,
            thread: Some(thread),
            stats,
        })
    }

    pub fn stats(&self) -> Stats {
        *self.stats.lock().unwrap()
    }

    pub fn stop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_recovers_one_loss_with_reordering_and_duplicates() {
        let data: Vec<u8> = (0..SHARD * 3 + 188).map(|i| (i * 31) as u8).collect();
        for missing in 0..5 {
            let p = packets(9, 0, &data).unwrap();
            let t = Instant::now();
            let mut r = Reorder::new(9);
            for i in (0..p.len()).rev().filter(|&i| i != missing) {
                r.push(&p[i], t).unwrap();
                r.push(&p[i], t).unwrap();
            }
            assert_eq!(r.drain(t).concat(), data);
            assert_eq!(r.stats.lost, 0);
        }
    }

    #[test]
    fn deadline_skips_unrecoverable_data_and_late_packets() {
        let t = Instant::now();
        let data = vec![0x47; SHARD * 4];
        let p = packets(7, 0, &data).unwrap();
        let mut r = Reorder::new(7);
        r.push(&p[2], t).unwrap();
        r.push(&p[3], t).unwrap();
        r.push(&p[4], t).unwrap();
        assert!(r.drain(t).is_empty());
        assert_eq!(r.drain(t + DEADLINE).concat(), data[SHARD * 2..]);
        assert_eq!(r.stats.lost, 2);
        r.push(&p[0], t + DEADLINE).unwrap();
        assert!(r.drain(t + DEADLINE).is_empty());
        // A datagram beyond the reorder window skips the gap instead of
        // permanently rejecting everything that follows it.
        assert!(r.push(&packets(7, 1000, &data).unwrap()[0], t).is_ok());
        assert!(r.stats.lost >= 999);
        assert!(r.push(&p[0][..20], t).is_err());
    }

    #[test]
    fn missing_group_does_not_hold_newer_video_forever() {
        let t = Instant::now();
        let data = vec![0x47; 188];
        let mut r = Reorder::new(8);
        r.push(&packets(8, 1, &data).unwrap()[0], t).unwrap();
        assert!(r.drain(t).is_empty());
        assert_eq!(r.drain(t + DEADLINE).concat(), data);
        assert_eq!(r.stats.lost, 1);
    }

    #[test]
    fn window_skip_purges_stale_groups_and_never_rewinds() {
        let t = Instant::now();
        // Two shards per group so one datagram alone leaves the group incomplete.
        let data = vec![0x47; SHARD + 188];
        let first = packets(11, 0, &data).unwrap();
        let far = packets(11, 2000, &data).unwrap();
        let mut r = Reorder::new(11);
        // Leave group 0 incomplete, then jump past the reorder window. The
        // first shard is emitted as soon as it is available.
        r.push(&first[0], t).unwrap();
        assert_eq!(r.drain(t).concat(), data[..SHARD]);
        r.push(&far[0], t).unwrap();
        assert_eq!(r.next, 2000);
        assert_eq!(r.stats.lost, 2000);
        r.push(&far[1], t).unwrap();
        assert_eq!(r.drain(t).concat(), data);
        assert_eq!(r.next, 2001);
        assert!(r.groups.is_empty());
        // A datagram from the abandoned range must not move the stream back.
        r.push(&first[1], t).unwrap();
        assert_eq!(r.next, 2001);
        assert!(r.drain(t).is_empty());
    }

    #[test]
    fn sender_flushes_only_whole_ts_packets_and_reassembles_in_order() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let sender_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut sender = Sender::new(sender_socket, receiver.local_addr().unwrap(), 42).unwrap();
        // Deliberately split in chunks that do not line up with shards or groups.
        let data: Vec<u8> = (0..188 * 40).map(|i| (i * 7) as u8).collect();
        for chunk in data.chunks(3000) {
            sender.write(chunk).unwrap();
        }
        // A partial TS packet must be retained, not sent as a truncated datagram.
        sender.write(&[0; 100]).unwrap();
        let mut reorder = Reorder::new(42);
        let mut buf = [0; HEADER + SHARD + 1];
        let mut received = 0;
        let mut dropped_one = false;
        let start = Instant::now();
        while let Ok(n) = receiver.recv(&mut buf) {
            received += 1;
            // Drop one data shard of a three-shard group; parity repairs it.
            if !dropped_one && buf[20] == 3 && buf[21] == 1 {
                dropped_one = true;
                continue;
            }
            reorder.push(&buf[..n], start).unwrap();
        }
        assert!(received > 0 && dropped_one);
        assert_eq!(reorder.drain(start + DEADLINE).concat(), data);
        assert_eq!(reorder.stats.lost, 0);
        assert_eq!(reorder.stats.recovered, 1);
    }
}
