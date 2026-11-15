//! RTP header building for the ZRTC relay — a byte-exact port of the Zalos
//! client's `engines/zcall-cpp/engine/src/wire/rtp.{h,cpp}` (BSD-3-Clause).
//!
//! Fixed header always `0x90` (2-byte CSRC count 0, extension present) with the
//! `0xbede` one-word extension marker, and **SSRC = the local Zalo uid**.
//! Extension ids the server expects: TWCC 5, audio-level 2, abs-send-time 3,
//! capture-NTP 13, SFU layer 12 (short) / 14 (long).

use super::bytes::{rd16be, rd32be, u8_at, wr16be, wr32be};

pub const AUDIO_PT: i32 = 112;
/// Opus 16 kHz, 20 ms frames.
pub const FRAME_SAMPLES: u32 = 320;

const TWCC_EXT_ID: i32 = 5;
const AUDIO_LEVEL_EXT_ID: i32 = 2;
const ABS_SEND_TIME_EXT_ID: i32 = 3;
const CAPTURE_NTP_EXT_ID: i32 = 13;
const NTP_UNIX_OFFSET: f64 = 2208988800.0;

/// `sfuByte0()` of the original is an env knob; production always sends 0x03.
const SFU_BYTE0: u8 = 0x03;

#[derive(Debug, Clone, Copy)]
pub struct RtpHeaderArgs {
    pub seq: u16,
    pub ts: u32,
    pub ssrc: u32,
    pub tw_seq: u16,
    pub marker: bool,
    pub pt: i32,
    pub layer_seq: u16,
    pub capture_ms: f64,
    pub send_ms: f64,
    pub level: i32,
}

impl Default for RtpHeaderArgs {
    fn default() -> Self {
        Self {
            seq: 0,
            ts: 0,
            ssrc: 0,
            tw_seq: 0,
            marker: false,
            pt: AUDIO_PT,
            layer_seq: 0,
            capture_ms: 0.0,
            send_ms: 0.0,
            level: 127,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SfuExt {
    pub id: i32,
    pub long_form: bool,
}

impl Default for SfuExt {
    fn default() -> Self {
        Self { id: 12, long_form: false }
    }
}

fn base_header(h: &mut [u8], a: &RtpHeaderArgs) {
    h[0] = 0x90;
    h[1] = (u8::from(a.marker) << 7) | ((a.pt & 0x7f) as u8);
    wr16be(h, 2, usize::from(a.seq));
    wr32be(h, 4, a.ts);
    wr32be(h, 8, a.ssrc);
    wr16be(h, 12, 0xbede);
}

/// Capture-NTP extension entry (id 13, 6 data bytes): 3 seconds bytes + 4 fraction.
fn write_capture_ntp(h: &mut [u8], o: usize, capture_ms: f64) -> usize {
    h[o] = ((CAPTURE_NTP_EXT_ID << 4) | 6) as u8;
    let ntp = capture_ms / 1000.0 + NTP_UNIX_OFFSET;
    let sec = ntp.floor();
    let frac = ((ntp - sec) * 4294967296.0).floor() as u64 & 0xffff_ffff;
    let s = sec as u64;
    h[o + 1] = ((s >> 16) & 0xff) as u8;
    h[o + 2] = ((s >> 8) & 0xff) as u8;
    h[o + 3] = (s & 0xff) as u8;
    wr32be(h, o + 4, frac as u32);
    o + 8
}

/// 1-1 audio: 20-byte header, one TWCC extension.
pub fn build_audio_rtp_header(a: &RtpHeaderArgs) -> Vec<u8> {
    let mut h = vec![0u8; 20];
    base_header(&mut h, a);
    wr16be(&mut h, 14, 1);
    h[16] = ((TWCC_EXT_ID << 4) | 1) as u8;
    wr16be(&mut h, 17, usize::from(a.tw_seq));
    h[19] = 0;
    h
}

/// Group audio: audio-level + TWCC + capture-NTP extensions.
pub fn build_group_audio_rtp_header(a: &RtpHeaderArgs) -> Vec<u8> {
    let mut h = vec![0u8; 12 + 4 + 16];
    base_header(&mut h, a);
    wr16be(&mut h, 14, 4);
    h[16] = ((AUDIO_LEVEL_EXT_ID << 4) | 0) as u8;
    h[17] = a.level.clamp(0, 127) as u8;
    h[18] = ((TWCC_EXT_ID << 4) | 1) as u8;
    wr16be(&mut h, 19, usize::from(a.tw_seq));
    write_capture_ntp(&mut h, 21, a.capture_ms);
    h
}

/// Group video: abs-send-time + TWCC + SFU layer + capture-NTP, padded to words.
pub fn build_group_video_rtp_header(a: &RtpHeaderArgs, sfu: &SfuExt) -> Vec<u8> {
    let lng = sfu.long_form;
    let id = if sfu.id != 0 { sfu.id } else if lng { 14 } else { 12 };
    let data_len = if lng { 7 } else { 3 };
    let ext_bytes = 4 + 3 + (1 + data_len) + 8;
    let padded = (ext_bytes + 3) & !3;
    let mut h = vec![0u8; 12 + 4 + padded];
    base_header(&mut h, a);
    wr16be(&mut h, 14, padded / 4);
    let mut o = 16;
    h[o] = ((ABS_SEND_TIME_EXT_ID << 4) | 2) as u8;
    let abs = (a.send_ms * 262.144).floor() as u64 % 0x100_0000;
    h[o + 1] = ((abs >> 16) & 0xff) as u8;
    h[o + 2] = ((abs >> 8) & 0xff) as u8;
    h[o + 3] = (abs & 0xff) as u8;
    o += 4;
    h[o] = ((TWCC_EXT_ID << 4) | 1) as u8;
    wr16be(&mut h, o + 1, usize::from(a.tw_seq));
    o += 3;
    h[o] = (((id & 0x0f) << 4) | (if lng { 6 } else { 2 })) as u8;
    h[o + 1] = SFU_BYTE0;
    wr16be(&mut h, o + 2, usize::from(a.layer_seq));
    o += 4; // id + byte0 + 2-byte layerSeq
    if lng {
        o += 4; // the long form carries 4 more zero bytes
    }
    write_capture_ntp(&mut h, o, a.capture_ms);
    h
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioRtpFields {
    pub marker: bool,
    pub pt: i32,
    pub seq: u16,
    pub ts: u32,
    pub ssrc: u32,
    pub tw_seq: Option<u16>,
}

/// Reads the fields of a 1-1 audio RTP header we produce (and the peer's).
pub fn read_audio_rtp_header(rtp: &[u8]) -> Option<AudioRtpFields> {
    if rtp.len() < 20 || u8_at(rtp, 0) != 0x90 || rd16be(rtp, 12) != 0xbede {
        return None;
    }
    let mut f = AudioRtpFields {
        marker: (u8_at(rtp, 1) & 0x80) != 0,
        pt: (u8_at(rtp, 1) & 0x7f) as i32,
        seq: rd16be(rtp, 2) as u16,
        ts: rd32be(rtp, 4),
        ssrc: rd32be(rtp, 8),
        tw_seq: None,
    };
    if (u8_at(rtp, 16) >> 4) == TWCC_EXT_ID as u32 {
        f.tw_seq = Some(rd16be(rtp, 17) as u16);
    }
    Some(f)
}

/// The audio-level extension value (0..=127 dBov) of an inbound group RTP packet.
pub fn rtp_audio_level(rtp: &[u8]) -> Option<i32> {
    if rtp.len() < 16 || (u8_at(rtp, 0) & 0x10) == 0 {
        return None;
    }
    let mut o = 12 + 4 * (u8_at(rtp, 0) & 0x0f) as usize;
    if o + 4 > rtp.len() || rd16be(rtp, o) != 0xbede {
        return None;
    }
    let end = rtp.len().min(o + 4 + 4 * rd16be(rtp, o + 2));
    o += 4;
    while o < end {
        let b = u8_at(rtp, o) as u8;
        if b == 0 {
            o += 1;
            continue;
        }
        let id = i32::from(b >> 4);
        let len = usize::from(b & 0x0f) + 1;
        if id == 15 {
            return None;
        }
        if id == AUDIO_LEVEL_EXT_ID && o + 1 < end {
            return Some((u8_at(rtp, o + 1) & 0x7f) as i32);
        }
        o += 1 + len;
    }
    None
}
