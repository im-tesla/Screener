use anyhow::{bail, ensure, Result};
use std::io::{Read, Write};

pub const MAGIC: &[u8; 8] = b"SCRNR004";
pub const MAX_PACKET: usize = 2 * 1024 * 1024;

/// Client hello capability byte: the viewer accepts desktop audio.
pub const CAPABILITY_AUDIO: u8 = 1 << 0;

/// Server status stream packet types: cursor, shape, info, stats, audio format, audio.
pub const STATUS_CURSOR: u8 = 0xC0;
pub const STATUS_CURSOR_SHAPE: u8 = 0xC1;
pub const STATUS_INFO: u8 = 0xC2;
pub const STATUS_STATS: u8 = 0xC3;
pub const STATUS_AUDIO_FORMAT: u8 = 0xC4;
pub const STATUS_AUDIO_DATA: u8 = 0xC5;

pub fn write_packet(writer: &mut impl Write, data: &[u8]) -> Result<()> {
    ensure!(data.len() <= MAX_PACKET, "packet exceeds limit");
    writer.write_all(&(data.len() as u32).to_le_bytes())?;
    writer.write_all(data)?;
    Ok(())
}

pub fn read_packet(reader: &mut impl Read, storage: &mut Vec<u8>) -> Result<()> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    ensure!(len > 0 && len <= MAX_PACKET, "invalid packet size {len}");
    storage.resize(len, 0);
    reader.read_exact(storage)?;
    Ok(())
}

pub fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn put_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.data.len().saturating_sub(self.pos) {
            bail!("truncated packet");
        }
        let b = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into()?))
    }
    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.bytes(4)?.try_into()?))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into()?))
    }
    pub fn remaining(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }
    pub fn finish(&self) -> Result<()> {
        ensure!(self.pos == self.data.len(), "trailing bytes");
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CursorShape {
    pub kind: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub bytes: Vec<u8>,
}
impl CursorShape {
    pub fn encode(&self, out: &mut Vec<u8>) {
        put_u32(out, self.kind);
        put_u32(out, self.width);
        put_u32(out, self.height);
        put_u32(out, self.pitch);
        put_i32(out, self.hot_x);
        put_i32(out, self.hot_y);
        put_u32(out, self.bytes.len() as u32);
        out.extend_from_slice(&self.bytes);
    }
    pub fn decode(r: &mut Reader) -> Result<Self> {
        let kind = r.u32()?;
        let width = r.u32()?;
        let height = r.u32()?;
        let pitch = r.u32()?;
        let hot_x = r.i32()?;
        let hot_y = r.i32()?;
        let len = r.u32()? as usize;
        ensure!(
            width > 0
                && width <= 512
                && height > 0
                && height <= 512
                && kind <= 1
                && pitch == width.div_ceil(16) * 2
                && hot_x >= 0
                && hot_y >= 0
                && hot_x < width as i32
                && hot_y < height as i32
                && len
                    == (if kind == 1 { width * height * 4 } else { 0 }) as usize
                        + (pitch * height * if kind == 0 { 2 } else { 1 }) as usize,
            "invalid cursor"
        );
        Ok(Self {
            kind,
            width,
            height,
            pitch,
            hot_x,
            hot_y,
            bytes: r.bytes(len)?.to_vec(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputEvent {
    Absolute { x: u16, y: u16 },
    Relative { x: i32, y: i32 },
    Button { button: u8, down: bool },
    Wheel { delta: i32, horizontal: bool },
    Key { scan: u16, vk: u16, flags: u8 }, // bit 0: up, bit 1: E0, bit 2: E1
    ReleaseAll,
    Heartbeat,
}
pub const INPUT_SIZE: usize = 12;
impl InputEvent {
    pub fn encode(self) -> [u8; INPUT_SIZE] {
        let mut b = [0; INPUT_SIZE];
        match self {
            Self::Absolute { x, y } => {
                b[0] = 1;
                b[1..3].copy_from_slice(&x.to_le_bytes());
                b[3..5].copy_from_slice(&y.to_le_bytes());
            }
            Self::Relative { x, y } => {
                b[0] = 2;
                b[1..5].copy_from_slice(&x.to_le_bytes());
                b[5..9].copy_from_slice(&y.to_le_bytes());
            }
            Self::Button { button, down } => {
                b[0] = 3;
                b[1] = button;
                b[2] = u8::from(down);
            }
            Self::Wheel { delta, horizontal } => {
                b[0] = 4;
                b[1..5].copy_from_slice(&delta.to_le_bytes());
                b[5] = u8::from(horizontal);
            }
            Self::Key { scan, vk, flags } => {
                b[0] = 5;
                b[1..3].copy_from_slice(&scan.to_le_bytes());
                b[3..5].copy_from_slice(&vk.to_le_bytes());
                b[5] = flags;
            }
            Self::ReleaseAll => b[0] = 6,
            Self::Heartbeat => b[0] = 7,
        }
        b
    }
    pub fn decode(b: &[u8; INPUT_SIZE]) -> Result<Self> {
        Ok(match b[0] {
            1 => Self::Absolute {
                x: u16::from_le_bytes(b[1..3].try_into()?),
                y: u16::from_le_bytes(b[3..5].try_into()?),
            },
            2 => Self::Relative {
                x: i32::from_le_bytes(b[1..5].try_into()?),
                y: i32::from_le_bytes(b[5..9].try_into()?),
            },
            3 if b[1] <= 4 => Self::Button {
                button: b[1],
                down: b[2] != 0,
            },
            4 => Self::Wheel {
                delta: i32::from_le_bytes(b[1..5].try_into()?),
                horizontal: b[5] != 0,
            },
            5 => Self::Key {
                scan: u16::from_le_bytes(b[1..3].try_into()?),
                vk: u16::from_le_bytes(b[3..5].try_into()?),
                flags: b[5],
            },
            6 => Self::ReleaseAll,
            7 => Self::Heartbeat,
            _ => bail!("invalid input event"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_roundtrips() {
        for e in [
            InputEvent::Absolute { x: 65535, y: 234 },
            InputEvent::Relative {
                x: -98,
                y: i32::MAX,
            },
            InputEvent::Button {
                button: 4,
                down: false,
            },
            InputEvent::Wheel {
                delta: -120,
                horizontal: true,
            },
            InputEvent::Key {
                scan: 0x1d,
                vk: 0xa3,
                flags: 3,
            },
            InputEvent::ReleaseAll,
            InputEvent::Heartbeat,
        ] {
            assert_eq!(InputEvent::decode(&e.encode()).unwrap(), e);
        }
    }
    #[test]
    fn framing_rejects_oversized_and_truncated() {
        assert!(read_packet(&mut &(u32::MAX.to_le_bytes())[..], &mut vec![]).is_err());
        assert!(read_packet(&mut &[4, 0, 0, 0, 1][..], &mut vec![]).is_err());
    }
}
