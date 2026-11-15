//! Big-endian byte helpers for the RTP/RTCP side of the wire (control headers
//! in `wire.rs` are little-endian; RTP is network order).

pub fn u8_at(buf: &[u8], at: usize) -> u32 {
    buf[at] as u32
}

pub fn rd16be(buf: &[u8], at: usize) -> usize {
    u16::from_be_bytes([buf[at], buf[at + 1]]) as usize
}

pub fn rd32be(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

pub fn rd24be(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([0, buf[at], buf[at + 1], buf[at + 2]])
}

/// Signed 16-bit big-endian (TWCC sequence deltas).
pub fn rd16sbe(buf: &[u8], at: usize) -> i64 {
    i16::from_be_bytes([buf[at], buf[at + 1]]) as i64
}

pub fn wr16be(buf: &mut [u8], at: usize, value: usize) {
    let v = (value as u16).to_be_bytes();
    buf[at..at + 2].copy_from_slice(&v);
}

pub fn wr24be(buf: &mut [u8], at: usize, value: u32) {
    let v = value.to_be_bytes();
    buf[at..at + 3].copy_from_slice(&v[1..4]);
}

pub fn wr32be(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_be_bytes());
}
