//! H.264 over the ZRTC video RTP — a byte-exact port of the Zalos client's
//! `engines/zcall-cpp/engine/src/wire/video.{h,cpp}` (BSD-3-Clause). Golden
//! vectors in `testdata/video.json` pin the packetizer, the SPS parser, the
//! `avc1.` codec string and both assembler modes (1-1 and group SFU).

use std::collections::{BTreeMap, HashMap};

use super::bytes::u8_at;

pub const KEY_PT: i32 = 98;
pub const DELTA_PT: i32 = 97;
pub const MAX_CHUNK: usize = 1068;

const REORDER_FRAMES: usize = 3;
const REORDER_MS: f64 = 200.0;
const MAX_PENDING: usize = 30;
const START_CODE: &[u8] = &[0x00, 0x00, 0x00, 0x01];

fn is_svc_nal(t: u8) -> bool {
    t == 14 || t == 15 || t == 20
}

fn nal_type(unit: &[u8]) -> u8 {
    u8_at(unit, 0) as u8 & 0x1f
}

pub fn is_fragment(p: &[u8]) -> bool {
    p.len() >= 2 && u8_at(p, 0) == 0x1c && (u8_at(p, 1) & 0x3f) == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureSize {
    pub width: i64,
    pub height: i64,
}

#[derive(Debug, Clone, Default)]
pub struct VideoPacket {
    pub seq: u16,
    pub ts: u32,
    pub pt: i32,
    pub marker: bool,
    pub payload: Vec<u8>,
    pub roc: u32,
    pub ssrc: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoFrame {
    pub key: bool,
    pub data: Vec<u8>,
    pub ts: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssemblerStats {
    pub frames: i32,
    pub dropped: i32,
    pub keys: i32,
    pub gaps: i32,
}

/// Free functions first — packetizing (sender side) and NAL utilities.

/// Splits an Annex-B frame into FU-A chunks (`0x1c` + start/end flags).
pub fn packetize_frame(annex_b: &[u8], max_chunk: usize) -> Vec<Vec<u8>> {
    if annex_b.len() <= max_chunk {
        return vec![annex_b.to_vec()];
    }
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < annex_b.len() {
        let end = (off + max_chunk).min(annex_b.len());
        let flag: u8 = if off == 0 {
            0x80
        } else if end == annex_b.len() {
            0x40
        } else {
            0x00
        };
        let mut chunk = Vec::with_capacity(2 + end - off);
        chunk.push(0x1c);
        chunk.push(flag);
        chunk.extend_from_slice(&annex_b[off..end]);
        out.push(chunk);
        off = end;
    }
    out
}

/// Splits an Annex-B stream into NAL units (3-byte start codes; trailing
/// zero-padding trimmed).
pub fn nal_units(annex_b: &[u8]) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 2 < annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 {
            starts.push(i + 3);
            i += 2;
        }
        i += 1;
    }
    let mut out = Vec::new();
    for (k, &st) in starts.iter().enumerate() {
        let mut end = if k + 1 < starts.len() { starts[k + 1] - 3 } else { annex_b.len() };
        while end > st && annex_b[end - 1] == 0 {
            end -= 1;
        }
        if end > st {
            out.push(annex_b[st..end].to_vec());
        }
    }
    out
}

/// `avc1.PPCCLL` from the first SPS start code in the Annex-B stream.
pub fn avc_codec_string(annex_b: &[u8]) -> String {
    let mut i = 0usize;
    while i + 7 < annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 && (u8_at(annex_b, i + 3) & 0x1f) == 7 {
            return format!("avc1.{}", hex(&annex_b[i + 4..i + 7]));
        }
        i += 1;
    }
    String::new()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Width/height out of an SPS (or SPS subset, NAL 15) payload — the minimal
/// bit-reader the renderer's codec string needs.
pub fn sps_size(nal: &[u8]) -> Option<PictureSize> {
    // Unescape emulation prevention bytes (00 00 03).
    let mut b: Vec<u8> = Vec::with_capacity(nal.len());
    for (i, &byte) in nal.iter().enumerate().skip(1) {
        if i >= 3 && byte == 3 && nal[i - 1] == 0 && nal[i - 2] == 0 {
            continue;
        }
        b.push(byte);
    }
    let mut pos = 0usize;
    macro_rules! bit {
        () => {{
            if pos >= b.len() * 8 {
                return None;
            }
            let v = (b[pos >> 3] >> (7 - (pos & 7))) & 1;
            pos += 1;
            i64::from(v)
        }};
    }
    macro_rules! bits {
        ($n:expr) => {{
            let mut v = 0i64;
            for _ in 0..$n {
                v = v * 2 + bit!();
            }
            v
        }};
    }
    macro_rules! ue {
        () => {{
            let mut z = 0;
            loop {
                if bit!() != 0 {
                    break;
                }
                z += 1;
                if z > 31 {
                    return None;
                }
            }
            bits!(z) + ((1i64 << z) - 1)
        }};
    }
    macro_rules! se {
        () => {{
            let k = ue!();
            if k & 1 == 1 {
                (k + 1) / 2
            } else {
                -(k / 2)
            }
        }};
    }
    let profile = bits!(8);
    bits!(16);
    ue!();
    let mut chroma = 1i64;
    const HIGH_PROFILES: [i64; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];
    if HIGH_PROFILES.contains(&profile) {
        chroma = ue!();
        if chroma == 3 {
            bit!();
        }
        ue!();
        ue!();
        bit!();
        if bit!() == 1 {
            for i in 0..(if chroma == 3 { 12 } else { 8 }) {
                if bit!() == 0 {
                    continue;
                }
                let mut last = 8i64;
                let mut next = 8i64;
                for _ in 0..(if i < 6 { 16 } else { 64 }) {
                    if next != 0 {
                        next = (last + se!() + 256) % 256;
                    }
                    last = if next != 0 { next } else { last };
                }
            }
        }
    }
    ue!();
    let poc = ue!();
    if poc == 0 {
        ue!();
    } else if poc == 1 {
        bit!();
        se!();
        se!();
        let n = ue!();
        for _ in 0..n {
            se!();
        }
    }
    ue!();
    bit!();
    let w_mbs = ue!() + 1;
    let h_map = ue!() + 1;
    let frame_mbs_only = bit!();
    if frame_mbs_only == 0 {
        bit!();
    }
    bit!();
    let (mut cl, mut cr, mut ct, mut cb) = (0i64, 0i64, 0i64, 0i64);
    if bit!() == 1 {
        cl = ue!();
        cr = ue!();
        ct = ue!();
        cb = ue!();
    }
    let cx = if chroma == 0 { 1 } else { 2 };
    let cy = (if chroma == 1 { 2 } else { 1 }) * (2 - frame_mbs_only);
    Some(PictureSize {
        width: w_mbs * 16 - cx * (cl + cr),
        height: (2 - frame_mbs_only) * h_map * 16 - cy * (ct + cb),
    })
}

// ------------------------------------------------------------------ assembler

#[derive(Default)]
struct Pending {
    parts: BTreeMap<i64, Vec<u8>>,
    key: bool,
    end: Option<i64>,
}

struct Ready {
    key: bool,
    data: Vec<u8>,
    ts: u32,
    last: i64,
    since: f64,
}

/// Reassembles FU-A fragmented H.264 into frames, orders them by the extended
/// sequence number, gates on keyframes (group mode) and re-injects cached
/// SPS/PPS before keyframes that lost theirs. The 1-1 path keeps the stream
/// as-is; the group path strips SVC NALs and tracks NAL byte counters.
pub struct VideoAssembler {
    on_frame: Box<dyn FnMut(VideoFrame)>,
    now: fn() -> f64,
    group: bool,
    on_gap: Box<dyn FnMut()>,
    pub stats: AssemblerStats,
    pub nal_bytes: HashMap<i32, i64>,
    pub sps_size7: String,
    pub sps_size15: String,
    pending: Vec<(u32, Pending)>,
    ready: BTreeMap<i64, Ready>,
    last_idx: i64,
    started: bool,
    need_key: bool,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl VideoAssembler {
    pub fn new(on_frame: Box<dyn FnMut(VideoFrame)>, now: fn() -> f64, group: bool, on_gap: Box<dyn FnMut()>) -> Self {
        Self {
            on_frame,
            now,
            group,
            on_gap,
            stats: AssemblerStats::default(),
            nal_bytes: HashMap::new(),
            sps_size7: String::new(),
            sps_size15: String::new(),
            pending: Vec::new(),
            ready: BTreeMap::new(),
            last_idx: -1,
            started: false,
            need_key: false,
            sps: Vec::new(),
            pps: Vec::new(),
        }
    }

    pub fn push(&mut self, pkt: VideoPacket) {
        if (!self.group && pkt.pt != DELTA_PT && pkt.pt != KEY_PT) || pkt.payload.is_empty() {
            return;
        }
        let idx = i64::from(pkt.roc) * 0x10000 + i64::from(pkt.seq);
        if idx <= self.last_idx {
            return;
        }
        if !self.find_pending(pkt.ts) {
            self.pending.push((pkt.ts, Pending::default()));
            if self.pending.len() > MAX_PENDING {
                self.pending.remove(0);
                self.stats.dropped += 1;
            }
            if !self.find_pending(pkt.ts) {
                self.flush();
                return;
            }
        }
        let entry = self.pending.iter_mut().find(|(ts, _)| *ts == pkt.ts).unwrap();
        entry.1.parts.insert(idx, pkt.payload.clone());
        if pkt.pt == KEY_PT {
            entry.1.key = true;
        }
        if pkt.marker {
            entry.1.end = Some(idx);
        }
        if entry.1.end.is_some() {
            self.complete(pkt.ts);
        }
        self.flush();
    }

    fn find_pending(&mut self, ts: u32) -> bool {
        self.pending.iter().any(|(t, _)| *t == ts)
    }

    fn complete(&mut self, ts: u32) {
        let Some(entry) = self.pending.iter().find(|(t, _)| *t == ts) else { return };
        let idxs: Vec<i64> = entry.1.parts.keys().copied().collect();
        if idxs.is_empty() {
            return;
        }
        let first = &entry.1.parts[&idxs[0]];
        if is_fragment(first) && u8_at(first, 1) != 0x80 {
            return; // missing the FU start
        }
        for w in idxs.windows(2) {
            if w[1] != w[0] + 1 {
                return; // a hole in this frame
            }
        }
        if entry.1.end != Some(*idxs.last().unwrap()) {
            return; // not the final chunk
        }
        let end = entry.1.end.unwrap();
        let key_hint = entry.1.key;
        let mut data: Vec<u8> = Vec::new();
        for i in &idxs {
            let p = &entry.1.parts[i];
            if is_fragment(p) {
                data.extend_from_slice(&p[2..]);
            } else {
                data.extend_from_slice(p);
            }
        }
        let mut key = key_hint;
        self.pending.retain(|(t, _)| *t != ts);
        if self.group {
            let units = nal_units(&data);
            let mut kept: Vec<Vec<u8>> = Vec::new();
            let mut has_sps = false;
            let mut has_pps = false;
            let mut has_idr = false;
            for u in &units {
                let t = nal_type(u);
                *self.nal_bytes.entry(i32::from(t)).or_insert(0) += u.len() as i64;
                if t == 7 || t == 15 {
                    if let Some(d) = sps_size(u) {
                        let s = format!("{}x{}", d.width, d.height);
                        if t == 7 {
                            self.sps_size7 = s;
                        } else {
                            self.sps_size15 = s;
                        }
                    }
                }
                if t == 7 || t == 5 {
                    has_idr = true;
                }
                if !is_svc_nal(t) {
                    if t == 7 {
                        has_sps = true;
                        self.sps = u.clone();
                    } else if t == 8 {
                        has_pps = true;
                        self.pps = u.clone();
                    }
                    kept.push(u.clone());
                }
            }
            key = has_idr;
            if kept.is_empty() {
                self.stats.dropped += 1;
                return;
            }
            if kept.len() != units.len() {
                data.clear();
                for u in &kept {
                    data.extend_from_slice(START_CODE);
                    data.extend_from_slice(u);
                }
            }
            if key && !self.sps.is_empty() && (!has_sps || (!has_pps && !self.pps.is_empty())) {
                let mut pre = Vec::new();
                if !has_sps {
                    pre.extend_from_slice(START_CODE);
                    pre.extend_from_slice(&self.sps);
                }
                if !has_pps && !self.pps.is_empty() {
                    pre.extend_from_slice(START_CODE);
                    pre.extend_from_slice(&self.pps);
                }
                let mut with_pre = pre;
                with_pre.extend_from_slice(&data);
                data = with_pre;
            }
        }
        let since = (self.now)();
        self.ready.insert(idxs[0], Ready { key, data, ts, last: end, since });
    }

    fn flush(&mut self) {
        loop {
            if self.ready.is_empty() {
                return;
            }
            let next_key = self.last_idx + 1;
            if self.last_idx >= 0 && self.ready.contains_key(&next_key) {
                let fr = self.ready.remove(&next_key).unwrap();
                self.emit_frame(fr);
                continue;
            }
            let (first_idx, oldest) = {
                let (k, v) = self.ready.iter().next().unwrap();
                (*k, Ready { key: v.key, data: v.data.clone(), ts: v.ts, last: v.last, since: v.since })
            };
            let waited = (self.now)() - oldest.since;
            if self.last_idx >= 0 && self.ready.len() <= REORDER_FRAMES && waited < REORDER_MS {
                return;
            }
            if !self.started && !oldest.key {
                self.ready.remove(&first_idx);
                self.stats.dropped += 1;
                continue;
            }
            if self.last_idx >= 0 {
                self.stats.gaps += 1;
                (self.on_gap)();
                if self.group {
                    self.need_key = true;
                }
            }
            self.ready.remove(&first_idx);
            self.emit_frame(oldest);
        }
    }

    fn emit_frame(&mut self, fr: Ready) {
        if self.group && self.need_key && !fr.key {
            self.last_idx = fr.last;
            self.stats.dropped += 1;
            return;
        }
        self.last_idx = fr.last;
        if fr.key {
            self.need_key = false;
        }
        self.pending.retain(|(_, p)| {
            let stale = p.parts.keys().next_back().is_some_and(|max| *max <= self.last_idx);
            if stale {
                self.stats.dropped += 1;
            }
            !stale
        });
        self.started = self.started || fr.key;
        self.stats.frames += 1;
        if fr.key {
            self.stats.keys += 1;
        }
        (self.on_frame)(VideoFrame { key: fr.key, data: fr.data, ts: fr.ts });
    }
}
