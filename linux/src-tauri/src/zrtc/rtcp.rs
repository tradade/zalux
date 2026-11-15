//! RTCP feedback as the ZRTC relay expects — a byte-exact port of the Zalos
//! client's `engines/zcall-cpp/engine/src/wire/rtcp.{h,cpp}` (BSD-3-Clause).
//!
//! TWCC feedback every ~50 ms, SR/RR + SDES every ~1 s, PLI rate-limited, NACK
//! per missing sequence with 40 ms retry / 5 tries / 400 ms drop. All cadence
//! decisions belong to the engine; these are the encoders and trackers.

use std::collections::{HashMap, HashSet, VecDeque};

use super::bytes::{rd16be, rd32be, u8_at, wr16be, wr24be, wr32be};

const NTP_EPOCH_OFFSET: f64 = 2208988800.0;
const MAX_TWCC_PACKETS: i64 = 400;
const NACK_MAX_HOLE: i64 = 64;
const NACK_RETRY_MS: f64 = 40.0;
const NACK_MAX_TRIES: i32 = 5;
const NACK_DROP_MS: f64 = 400.0;
const NACK_KEEP: usize = 128;

pub fn js_round(v: f64) -> i64 {
    (v + 0.5).floor() as i64
}

// ------------------------------------------------------------------ TWCC

pub fn read_tw_seq(rtp: &[u8]) -> Option<u16> {
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
        if id == 5 && len == 2 && o + 3 <= end {
            return Some(rd16be(rtp, o + 1) as u16);
        }
        o += 1 + len;
    }
    None
}

pub struct TwccRecorder {
    arrivals: HashMap<i64, i64>,
    top: Option<i64>,
    next: Option<i64>,
    fb_count: i32,
    sent: i32,
}

impl Default for TwccRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl TwccRecorder {
    pub fn new() -> Self {
        Self { arrivals: HashMap::new(), top: None, next: None, fb_count: 0, sent: 0 }
    }

    fn unwrap(&self, seq: i64) -> i64 {
        let Some(top) = self.top else { return seq };
        let mut u = seq + (top - (top & 0xffff));
        if u - top > 0x8000 {
            u -= 0x10000;
        } else if top - u > 0x8000 {
            u += 0x10000;
        }
        u
    }

    pub fn add(&mut self, seq: u32, now_ms: f64) {
        let u = self.unwrap(i64::from(seq & 0xffff));
        if self.next.is_none() {
            self.next = Some(u);
        }
        if self.next.is_some_and(|n| u < n) || self.arrivals.contains_key(&u) {
            return;
        }
        self.arrivals.insert(u, js_round(now_ms * 4.0));
        if self.top.is_none_or(|t| u > t) {
            self.top = Some(u);
        }
    }

    pub fn pending(&self) -> bool {
        self.next.is_some_and(|n| self.top.is_some_and(|t| t >= n))
    }

    pub fn build(&mut self, sender_ssrc: u32, media_ssrc: u32) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while self.pending() {
            let base = self.next.unwrap();
            let end = self.top.unwrap().min(base + MAX_TWCC_PACKETS - 1);
            let any = (base..=end).any(|s| self.arrivals.contains_key(&s));
            if !any {
                self.next = Some(end + 1);
                continue;
            }
            let fb = self.fb_count & 0xff;
            self.fb_count += 1;
            let arrivals = &self.arrivals;
            out.push(encode_twcc(sender_ssrc, media_ssrc, base, end, arrivals, fb));
            for s in base..=end {
                self.arrivals.remove(&s);
            }
            self.next = Some(end + 1);
            self.sent += 1;
        }
        out
    }

    pub fn sent(&self) -> i32 {
        self.sent
    }

    pub fn seen_any(&self) -> bool {
        self.top.is_some()
    }
}

pub fn encode_twcc(
    sender_ssrc: u32, media_ssrc: u32, base: i64, end: i64, arrivals: &HashMap<i64, i64>, fb_count: i32,
) -> Vec<u8> {
    let count = end - base + 1;
    let mut first_q = 0i64;
    for s in base..=end {
        if let Some(q) = arrivals.get(&s) {
            first_q = *q;
            break;
        }
    }
    let ref_units = (first_q as f64 / 256.0).floor() as i64;
    let ref_q = ref_units * 256;
    let mut symbols: Vec<i32> = Vec::with_capacity(count as usize);
    let mut deltas: Vec<i64> = Vec::new();
    let mut prev_q = ref_q;
    for s in base..=end {
        match arrivals.get(&s) {
            None => symbols.push(0),
            Some(&q) => {
                let d = q - prev_q;
                prev_q = q;
                if (0..=255).contains(&d) {
                    symbols.push(1);
                    deltas.push(d);
                } else {
                    symbols.push(2);
                    deltas.push(d.clamp(-32768, 32767));
                }
            }
        }
    }
    let mut chunks: Vec<u16> = Vec::new();
    let mut i = 0usize;
    while i < symbols.len() {
        let mut run = 1usize;
        while i + run < symbols.len() && symbols[i + run] == symbols[i] && run < 0x1fff {
            run += 1;
        }
        if run >= 8 {
            chunks.push(((symbols[i] as u16) << 13) | run as u16);
            i += run;
            continue;
        }
        let mut v: u32 = 0xc000;
        for k in 0..7 {
            let sym = if i + k < symbols.len() { symbols[i + k] } else { 0 };
            v |= (sym as u32) << (12 - 2 * k);
        }
        chunks.push(v as u16);
        i += 7;
    }
    let delta_bytes: usize = symbols.iter().map(|&s| if s == 1 { 1 } else if s == 2 { 2 } else { 0 }).sum();
    let len = 4 + 8 + 8 + 2 * chunks.len() + delta_bytes;
    let padded = (len + 3) & !3;
    let mut b = vec![0u8; padded];
    b[0] = 0x8f;
    b[1] = 205;
    wr16be(&mut b, 2, padded / 4 - 1);
    wr32be(&mut b, 4, sender_ssrc);
    wr32be(&mut b, 8, media_ssrc);
    wr16be(&mut b, 12, (base & 0xffff) as usize);
    wr16be(&mut b, 14, count as usize);
    wr24be(&mut b, 16, (ref_units & 0xff_ffff) as u32);
    b[19] = (fb_count & 0xff) as u8;
    let mut o = 20;
    for c in chunks {
        wr16be(&mut b, o, usize::from(c));
        o += 2;
    }
    let mut di = 0usize;
    for &s in &symbols {
        if s == 1 {
            b[o] = deltas[di] as u8;
            di += 1;
            o += 1;
        } else if s == 2 {
            let d = deltas[di] as i16;
            wr16be(&mut b, o, usize::from(d as u16));
            di += 1;
            o += 2;
        }
    }
    b
}

#[derive(Debug, Clone, PartialEq)]
pub struct TwccFeedback {
    pub sender_ssrc: u32,
    pub media_ssrc: u32,
    pub base: u16,
    pub count: u16,
    pub ref_units: u32,
    pub fb_count: u8,
    pub status: Vec<i32>,
    pub delta_q: Vec<i64>,
}

pub fn parse_twcc(b: &[u8]) -> Option<TwccFeedback> {
    if b.len() < 20 || (u8_at(b, 0) & 0x1f) != 15 || u8_at(b, 1) != 205 {
        return None;
    }
    let mut out = TwccFeedback {
        sender_ssrc: rd32be(b, 4),
        media_ssrc: rd32be(b, 8),
        base: rd16be(b, 12) as u16,
        count: rd16be(b, 14) as u16,
        ref_units: 0,
        fb_count: 0,
        status: Vec::new(),
        delta_q: Vec::new(),
    };
    out.ref_units = {
        // rd24be
        u32::from_be_bytes([0, b[16], b[17], b[18]])
    };
    out.fb_count = b[19];
    let mut o = 20usize;
    while out.status.len() < out.count as usize && o + 2 <= b.len() {
        let c = rd16be(b, o) as u16;
        o += 2;
        if (c & 0x8000) == 0 {
            let sym = (c >> 13) & 3;
            let run = c & 0x1fff;
            for _ in 0..run {
                if out.status.len() < out.count as usize {
                    out.status.push(i32::from(sym));
                }
            }
        } else if c & 0x4000 != 0 {
            for k in 0..7 {
                if out.status.len() < out.count as usize {
                    out.status.push(i32::from((c >> (12 - 2 * k)) & 3));
                }
            }
        } else {
            for k in 0..14 {
                if out.status.len() < out.count as usize {
                    out.status.push(i32::from((c >> (13 - k)) & 1));
                }
            }
        }
    }
    for &s in &out.status {
        match s {
            1 => {
                if o >= b.len() {
                    break;
                }
                out.delta_q.push(i64::from(b[o]));
                o += 1;
            }
            2 => {
                if o + 2 > b.len() {
                    break;
                }
                out.delta_q.push(i16::from_be_bytes([b[o], b[o + 1]]) as i64);
                o += 2;
            }
            _ => {}
        }
    }
    Some(out)
}

// ------------------------------------------------------------------ SR / RR / SDES / PLI / NACK

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtpTime {
    pub msw: u32,
    pub lsw: u32,
}

pub fn ntp_now(now_ms: f64) -> NtpTime {
    let sec = (now_ms / 1000.0).floor();
    let frac = ((now_ms % 1000.0) / 1000.0 * 4294967296.0).floor();
    NtpTime { msw: ((sec + NTP_EPOCH_OFFSET) as u64 & 0xffff_ffff) as u32, lsw: (frac as u64 & 0xffff_ffff) as u32 }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReportBlock {
    pub ssrc: u32,
    pub fraction_lost: i32,
    pub cum_lost: i64,
    pub ext_high_seq: u32,
    pub jitter: u32,
    pub lsr: u32,
    pub dlsr: u32,
}

fn report_block(buf: &mut [u8], o: usize, blk: &ReportBlock) {
    wr32be(buf, o, blk.ssrc);
    buf[o + 4] = (blk.fraction_lost & 0xff) as u8;
    wr24be(buf, o + 5, (blk.cum_lost as u32) & 0xff_ffff);
    wr32be(buf, o + 8, blk.ext_high_seq);
    wr32be(buf, o + 12, blk.jitter);
    wr32be(buf, o + 16, blk.lsr);
    wr32be(buf, o + 20, blk.dlsr);
}

pub fn build_sr(ssrc: u32, now_ms: f64, rtp_ts: u32, packets: u32, octets: u32, blocks: &[ReportBlock]) -> Vec<u8> {
    let mut b = vec![0u8; 28 + 24 * blocks.len()];
    b[0] = 0x80 | blocks.len() as u8;
    b[1] = 200;
    let length = b.len() / 4 - 1;
    wr16be(&mut b, 2, length);
    wr32be(&mut b, 4, ssrc);
    let n = ntp_now(now_ms);
    wr32be(&mut b, 8, n.msw);
    wr32be(&mut b, 12, n.lsw);
    wr32be(&mut b, 16, rtp_ts);
    wr32be(&mut b, 20, packets);
    wr32be(&mut b, 24, octets);
    for (i, blk) in blocks.iter().enumerate() {
        report_block(&mut b, 28 + 24 * i, blk);
    }
    b
}

pub fn build_rr(ssrc: u32, blocks: &[ReportBlock]) -> Vec<u8> {
    let mut b = vec![0u8; 8 + 24 * blocks.len()];
    b[0] = 0x80 | blocks.len() as u8;
    b[1] = 201;
    let length = b.len() / 4 - 1;
    wr16be(&mut b, 2, length);
    wr32be(&mut b, 4, ssrc);
    for (i, blk) in blocks.iter().enumerate() {
        report_block(&mut b, 8 + 24 * i, blk);
    }
    b
}

pub fn build_sdes(ssrc: u32) -> Vec<u8> {
    let mut b = vec![0u8; 12];
    b[0] = 0x81;
    b[1] = 202;
    wr16be(&mut b, 2, 2);
    wr32be(&mut b, 4, ssrc);
    b[8] = 1;
    b
}

pub fn build_pli(sender_ssrc: u32, media_ssrc: u32) -> Vec<u8> {
    let mut b = vec![0u8; 12];
    b[0] = 0x81;
    b[1] = 206;
    wr16be(&mut b, 2, 2);
    wr32be(&mut b, 4, sender_ssrc);
    wr32be(&mut b, 8, media_ssrc);
    b
}

pub fn build_nack(sender_ssrc: u32, media_ssrc: u32, seqs: &[u16]) -> Vec<u8> {
    let mut items: Vec<(u16, u16)> = Vec::new();
    let mut i = 0usize;
    while i < seqs.len() {
        let pid = seqs[i];
        let mut blp: u16 = 0;
        let mut j = i + 1;
        while j < seqs.len() {
            let d = (i64::from(seqs[j]) - i64::from(pid)) & 0xffff;
            if !(1..=16).contains(&d) {
                break;
            }
            blp |= 1 << (d - 1);
            j += 1;
        }
        items.push((pid, blp));
        i = j;
    }
    let mut b = vec![0u8; 12 + 4 * items.len()];
    b[0] = 0x81;
    b[1] = 205;
    let length = b.len() / 4 - 1;
    wr16be(&mut b, 2, length);
    wr32be(&mut b, 4, sender_ssrc);
    wr32be(&mut b, 8, media_ssrc);
    for (k, (pid, blp)) in items.iter().enumerate() {
        wr16be(&mut b, 12 + 4 * k, usize::from(*pid));
        wr16be(&mut b, 14 + 4 * k, usize::from(*blp));
    }
    b
}

// ------------------------------------------------------------------ NACK tracker

#[derive(Debug, Clone, PartialEq)]
pub struct NackRequest {
    pub ssrc: u32,
    pub seqs: Vec<u16>,
}

#[derive(Default)]
struct NackStream {
    highest: u16,
    miss: Vec<(u16, Miss)>,
}

#[derive(Clone, Copy)]
struct Miss {
    at: f64,
    tries: i32,
}

/// Tracks inbound video sequence gaps and emits (ssrc, missing seqs) when a
/// hole is due for re-request: 40 ms first retry, doubling cadence via the
/// tries counter, dropped after 400 ms, at most 5 tries, ≤128 holes kept.
#[derive(Default)]
pub struct NackTracker {
    streams: HashMap<u32, NackStream>,
}

impl NackTracker {
    pub fn note(&mut self, ssrc: u32, seq: u16, now: f64) -> Vec<NackRequest> {
        if !self.streams.contains_key(&ssrc) {
            let st = NackStream { highest: seq, miss: Vec::new() };
            self.streams.insert(ssrc, st);
            return Vec::new();
        }
        let st = self.streams.get_mut(&ssrc).unwrap();
        let ahead = (i64::from(seq).wrapping_sub(i64::from(st.highest))) & 0xffff;
        if ahead == 0 {
            return Self::due(st, ssrc, now);
        }
        if ahead >= 0x8000 {
            st.miss.retain(|(s, _)| *s != seq);
            return Self::due(st, ssrc, now);
        }
        if ahead > NACK_MAX_HOLE {
            st.highest = seq;
            st.miss.clear();
            return Vec::new();
        }
        for d in 1..ahead {
            let missing = (st.highest as i64 + d) as u16;
            if !st.miss.iter().any(|(s, _)| *s == missing) {
                st.miss.push((missing, Miss { at: now, tries: 0 }));
            }
        }
        st.highest = seq;
        st.miss.retain(|(s, _)| *s != seq);
        while st.miss.len() > NACK_KEEP {
            st.miss.remove(0);
        }
        Self::due(st, ssrc, now)
    }

    fn due(st: &mut NackStream, ssrc: u32, now: f64) -> Vec<NackRequest> {
        let mut seqs = Vec::new();
        st.miss.retain_mut(|(s, info)| {
            if now - info.at > NACK_DROP_MS {
                return false;
            }
            if info.tries >= NACK_MAX_TRIES || now < info.at + f64::from(info.tries) * NACK_RETRY_MS {
                return true;
            }
            info.tries += 1;
            seqs.push(*s);
            true
        });
        if seqs.is_empty() {
            return Vec::new();
        }
        let high = st.highest;
        seqs.sort_by_key(|&a| std::cmp::Reverse((i64::from(high) - i64::from(a)) & 0xffff));
        vec![NackRequest { ssrc, seqs }]
    }
}

// ------------------------------------------------------------------ RxStats + RtcpFeedback

/// Per-stream reception stats (RFC 3550 style) feeding SR/RR report blocks.
pub struct RxStats {
    clock_hz: f64,
    ssrc: u32,
    started: bool,
    base_seq: i64,
    max_seq: i64,
    cycles: i64,
    received: i64,
    expected_prior: i64,
    received_prior: i64,
    jitter: f64,
    transit: Option<f64>,
    seen: HashSet<i64>,
    seen_order: VecDeque<i64>,
    lsr: u32,
    lsr_at: f64,
}

impl RxStats {
    pub fn new(clock_hz: f64) -> Self {
        Self {
            clock_hz,
            ssrc: 0,
            started: false,
            base_seq: 0,
            max_seq: 0,
            cycles: 0,
            received: 0,
            expected_prior: 0,
            received_prior: 0,
            jitter: 0.0,
            transit: None,
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            lsr: 0,
            lsr_at: 0.0,
        }
    }

    pub fn on_packet(&mut self, rtp: &[u8], now_ms: f64) {
        if rtp.len() < 12 {
            return;
        }
        let seq = rd16be(rtp, 2) as i64;
        let ts = rd32be(rtp, 4) as i64;
        if !self.started {
            self.started = true;
            self.ssrc = rd32be(rtp, 8);
            self.base_seq = seq;
            self.max_seq = seq;
        } else {
            let d = (seq - self.max_seq) & 0xffff;
            if d != 0 && d < 0x8000 {
                if seq < self.max_seq {
                    self.cycles += 0x10000;
                }
                self.max_seq = seq;
            }
        }
        let ext = self.cycles + seq;
        if self.seen.contains(&ext) {
            return;
        }
        self.seen.insert(ext);
        self.seen_order.push_back(ext);
        if self.seen.len() > 512 {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen.remove(&old);
            }
        }
        self.received += 1;
        let arrival = now_ms * self.clock_hz / 1000.0;
        let transit = arrival - ts as f64;
        if let Some(previous) = self.transit {
            let dd = (transit - previous).abs();
            self.jitter += (dd - self.jitter) / 16.0;
        }
        self.transit = Some(transit);
    }

    pub fn on_sr(&mut self, buf: &[u8], now_ms: f64) {
        if buf.len() < 28 {
            return;
        }
        self.lsr = ((rd32be(buf, 8) & 0xffff) << 16) | (rd32be(buf, 12) >> 16);
        self.lsr_at = now_ms;
    }

    pub fn block(&mut self, now_ms: f64) -> Option<ReportBlock> {
        if !self.started {
            return None;
        }
        let ext_high = self.cycles + self.max_seq;
        let expected = ext_high - self.base_seq + 1;
        let lost = expected - self.received;
        let exp_int = expected - self.expected_prior;
        let rec_int = self.received - self.received_prior;
        self.expected_prior = expected;
        self.received_prior = self.received;
        let lost_int = exp_int - rec_int;
        let fraction = if exp_int <= 0 || lost_int <= 0 { 0 } else { (lost_int * 256 / exp_int).min(255) as i32 };
        Some(ReportBlock {
            ssrc: self.ssrc,
            fraction_lost: fraction,
            cum_lost: lost.clamp(-0x80_0000, 0x7f_ffff),
            ext_high_seq: (ext_high & 0xffff_ffff) as u32,
            jitter: js_round(self.jitter) as u32,
            lsr: self.lsr,
            dlsr: if self.lsr_at != 0.0 { (js_round((now_ms - self.lsr_at) * 65536.0 / 1000.0) as u64 & 0xffff_ffff) as u32 } else { 0 },
        })
    }

    pub fn started(&self) -> bool {
        self.started
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RtcpCounters {
    pub twcc: i32,
    pub reports: i32,
    pub pli: i32,
    pub nack: i32,
}

/// The per-call feedback engine: feeds inbound RTP/RTCP, produces the TWCC
/// batches, the periodic SR/RR reports and the PLI requests.
pub struct RtcpFeedback {
    local_ssrc: u32,
    twcc: TwccRecorder,
    audio: RxStats,
    video: RxStats,
    peer_ssrc: u32,
    counters: RtcpCounters,
}

impl RtcpFeedback {
    pub fn new(local_ssrc: u32) -> Self {
        Self {
            local_ssrc,
            twcc: TwccRecorder::new(),
            audio: RxStats::new(16000.0),
            video: RxStats::new(90000.0),
            peer_ssrc: 0,
            counters: RtcpCounters::default(),
        }
    }

    pub fn on_rtp(&mut self, video: bool, rtp: &[u8], now_ms: f64) {
        if rtp.len() < 12 {
            return;
        }
        if self.peer_ssrc == 0 {
            self.peer_ssrc = rd32be(rtp, 8);
        }
        if let Some(tw) = read_tw_seq(rtp) {
            self.twcc.add(u32::from(tw), now_ms);
        }
        if video {
            self.video.on_packet(rtp, now_ms);
        } else {
            self.audio.on_packet(rtp, now_ms);
        }
    }

    pub fn on_rtcp(&mut self, video: bool, buf: &[u8], now_ms: f64) {
        let mut o = 0usize;
        while o + 4 <= buf.len() {
            if u8_at(buf, o) >> 6 != 2 {
                return;
            }
            let len = (rd16be(buf, o + 2) + 1) * 4;
            if u8_at(buf, o + 1) == 200 && o + len <= buf.len() {
                if video {
                    self.video.on_sr(&buf[o..o + len], now_ms);
                } else {
                    self.audio.on_sr(&buf[o..o + len], now_ms);
                }
            }
            o += len;
        }
    }

    pub fn twcc_packets(&mut self) -> Vec<Vec<u8>> {
        if self.peer_ssrc == 0 {
            return Vec::new();
        }
        let pk = self.twcc.build(self.local_ssrc, self.peer_ssrc);
        self.counters.twcc += pk.len() as i32;
        pk
    }

    pub fn audio_report(&mut self, now_ms: f64, rtp_ts: u32, packets: u32, octets: u32) -> Vec<u8> {
        let blk = self.audio.block(now_ms);
        self.counters.reports += 1;
        let blocks = blk.into_iter().collect::<Vec<_>>();
        let mut out = build_sr(self.local_ssrc, now_ms, rtp_ts, packets, octets, &blocks);
        out.extend_from_slice(&build_sdes(self.local_ssrc));
        out
    }

    pub fn video_report(&mut self, now_ms: f64) -> Option<Vec<u8>> {
        let blk = self.video.block(now_ms)?;
        self.counters.reports += 1;
        let mut out = build_rr(self.local_ssrc, &[blk]);
        out.extend_from_slice(&build_sdes(self.local_ssrc));
        Some(out)
    }

    pub fn pli(&mut self) -> Option<Vec<u8>> {
        if !self.video.started() {
            return None;
        }
        self.counters.pli += 1;
        Some(build_pli(self.local_ssrc, self.video.ssrc()))
    }

    pub fn peer_ssrc(&self) -> u32 {
        self.peer_ssrc
    }

    pub fn counters(&self) -> RtcpCounters {
        self.counters
    }

    pub fn video(&mut self) -> &mut RxStats {
        &mut self.video
    }
}
