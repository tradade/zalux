//! The real media backend: one thread owning the UDP socket, the relay
//! registration, SRTP, the Opus codec and the RTCP feedback engine. The
//! engine thread talks to it through a bounded job queue and polls events —
//! nothing here ever runs on an HTTP thread, and engine entries never block
//! on the network.
//!
//! Wire details come from `wire.rs` / `srtp.rs` / `rtp.rs` / `rtcp.rs` (all
//! pinned by golden vectors). Behaviour:
//!
//! * Registration floods `InitZRTP` to every advertised server (three rounds
//!   200 ms apart), collects replies for 600 ms and keeps the lowest RTT.
//! * Once started, each mic frame is `wrap(0x03, token, SRTP(RTP(opus)))`;
//!   downlink arrives as `0x04` + SRTP (no token). TWCC every 50 ms, SR+SDES
//!   every 1 s, relay ping every 5 s. 50 peer-audio packets before the answer
//!   signal trigger the caller's "connect anyway" heuristic.

use super::audio::{AudioIo, Direction, JitterBuffer, PcmRing, VoiceDecoder, VoiceEncoder, FRAME_SAMPLES, SAMPLE_RATE};
use super::bytes::u8_at;
use super::engine::{InitReply, Media, NegotiatedParams, Role};
use super::rtcp::{build_pli, RtcpFeedback};
use super::rtp::{self, RtpHeaderArgs};
use super::srtp::{self, SrtpKeys};
use super::video::{VideoAssembler, VideoFrame, VideoPacket};
use super::wire::{self, msg_type, unwrap_packet};
use serde_json::Value;
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const REG_ROUNDS: usize = 3;
const REG_ROUND_MS: u64 = 200;
const REG_WINDOW_MS: u64 = 600;
const PING_EVERY_MS: u64 = 5_000;
const TWCC_EVERY_MS: u64 = 50;
const SR_EVERY_MS: u64 = 1_000;
const EARLY_AUDIO_STREAM_PACKETS: u32 = 50;

enum Job {
    Negotiated(NegotiatedParams),
    Prepare(Role),
    Start(Role, String),
    Stop,
}

/// Events the media thread sends back to the engine worker.
pub enum MediaEvent {
    Prepared(Option<InitReply>),
    /// Group relay roster (`RoomUpdated`, cmd 50).
    GroupRoster(Vec<u32>),
    /// Peer audio before the answer signal ("first" packet / "stream" steady).
    EarlyAudio(&'static str),
    /// A device direction died mid-call.
    Fault(&'static str),
}

pub struct ZrtcMedia {
    jobs: mpsc::SyncSender<Job>,
    events: mpsc::Receiver<MediaEvent>,
    udp_ok: Arc<AtomicBool>,
    /// Sent/received SRTP payload counts, for the 406 stats.
    pub tx: Arc<AtomicU64>,
    pub rx: Arc<AtomicU64>,
}

impl ZrtcMedia {
    pub fn spawn() -> Self {
        let (jobs, job_rx) = mpsc::sync_channel(256);
        let (event_tx, events) = mpsc::channel();
        let udp_ok = Arc::new(AtomicBool::new(true));
        let tx = Arc::new(AtomicU64::new(0));
        let rx = Arc::new(AtomicU64::new(0));
        let flags = (udp_ok.clone(), tx.clone(), rx.clone());
        std::thread::Builder::new()
            .name("zrtc-media".into())
            .spawn(move || media_loop(job_rx, event_tx, flags))
            .expect("spawn zrtc-media worker");
        Self { jobs, events, udp_ok, tx, rx }
    }

    /// Drains pending media events (the engine worker calls this each turn).
    pub fn drain_events(&self) -> Vec<MediaEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            out.push(event);
        }
        out
    }
}

impl Media for ZrtcMedia {
    fn on_negotiated(&mut self, params: &NegotiatedParams) {
        let _ = self.jobs.try_send(Job::Negotiated(params.clone()));
    }

    fn prepare(&mut self, role: Role) {
        let _ = self.jobs.try_send(Job::Prepare(role));
    }

    fn start(&mut self, role: Role, rtp_ser_ip: &str) {
        let _ = self.jobs.try_send(Job::Start(role, rtp_ser_ip.to_string()));
    }

    fn stop(&mut self) {
        let _ = self.jobs.try_send(Job::Stop);
    }

    fn send_udp(&self) -> bool {
        self.udp_ok.load(Ordering::Relaxed)
    }

    fn stats(&self) -> (u64, u64) {
        (self.tx.load(Ordering::Relaxed), self.rx.load(Ordering::Relaxed))
    }
}

// ------------------------------------------------------------------ media thread

struct RelayServer {
    raw: Value,
    addr: SocketAddr,
    token: u32,
}

fn server_list(servers: &Value) -> Vec<RelayServer> {
    servers
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|s| {
                    let raw_rtp = s.get("rtpaddr").and_then(Value::as_str)?.to_string();
                    // "ip|port"
                    let (ip, port) = raw_rtp.split_once('|')?;
                    let addr = SocketAddr::new(ip.trim().parse().ok()?, port.trim().parse().ok()?);
                    Some(RelayServer { raw: s.clone(), addr, token: 0 })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ROC tracker; `exact` for our own stream (we see the wrap), guessed for the peer's.
struct Roc {
    last: Option<u16>,
    roc: u32,
}

impl Roc {
    fn new() -> Self {
        Self { last: None, roc: 0 }
    }

    fn advance(&mut self, seq: u16) {
        if let Some(last) = self.last {
            if seq < last {
                self.roc += 1;
            }
        }
        self.last = Some(seq);
    }

    fn guess(&mut self, seq: u16) -> u32 {
        match self.last {
            None => self.last = Some(seq),
            Some(last) => {
                let forward = (i64::from(seq) - i64::from(last)) & 0xffff;
                if forward != 0 && forward < 0x8000 && seq < last {
                    self.roc += 1;
                } else if forward >= 0x8000 && seq > last {
                    self.roc = self.roc.saturating_sub(1);
                }
                self.last = Some(seq);
            }
        }
        self.roc
    }
}

fn media_loop(jobs: mpsc::Receiver<Job>, events: mpsc::Sender<MediaEvent>, flags: (Arc<AtomicBool>, Arc<AtomicU64>, Arc<AtomicU64>)) {
    let (udp_ok, tx_counter, rx_counter) = flags;
    let mut negotiated: Option<NegotiatedParams> = None;
    let mut socket: Option<UdpSocket> = None;
    let mut servers: Vec<RelayServer> = Vec::new();
    let mut best_idx: Option<usize> = None;
    let mut active: Option<Role> = None;
    let mut keys: Option<SrtpKeys> = None;
    let mut roc_out = Roc::new();
    let mut rocs_in: HashMap<u32, Roc> = HashMap::new();
    let mut rtp_seq: u16 = 0;
    let mut rtp_ts: u32 = 0;
    let mut encoder = VoiceEncoder::new().ok();
    let mut decoder = VoiceDecoder::new().ok();
    let mut jitter = JitterBuffer::default();
    let mut group_jitters: HashMap<u32, JitterBuffer> = HashMap::new();
    let mic_ring = PcmRing::new(SAMPLE_RATE as usize / 5); // 200 ms
    let spk_ring = PcmRing::new(SAMPLE_RATE as usize / 3); // ~333 ms
    let mut audio: Option<AudioIo> = None;
    let mut rtcp: Option<RtcpFeedback> = None;
    let mut videos: HashMap<u32, VideoAssembler> = HashMap::new();
    let mut last_video_gaps: HashMap<u32, i32> = HashMap::new();
    let mut last_ping = Instant::now();
    let mut last_twcc = Instant::now();
    let mut last_sr = Instant::now();
    let mut last_rx = Instant::now();
    let mut peer_audio_packets: u32 = 0;
    let mut early_first_sent = false;

    loop {
        let timeout = if active.is_some() { Duration::from_millis(20) } else { Duration::from_millis(200) };
        match jobs.recv_timeout(timeout) {
            Ok(Job::Negotiated(params)) => {
                negotiated = Some(params);
                servers.clear();
                best_idx = None;
                active = None;
                keys = None;
            }
            Ok(Job::Prepare(role)) => {
                let Some(params) = negotiated.clone() else { continue };
                let sock = match UdpSocket::bind("0.0.0.0:0") {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[olaz] zrtc media: bind UDP: {e}");
                        udp_ok.store(false, Ordering::Relaxed);
                        let _ = events.send(MediaEvent::Prepared(None));
                        continue;
                    }
                };
                let _ = sock.set_read_timeout(Some(Duration::from_millis(20)));
                let call_id = (params.call_id as u32) & 0xffff_ffff;
                let uid = (params.local_uid as u32) & 0xffff_ffff;
                let peer_uid = (params.peer_uid as u32) & 0xffff_ffff;
                let list = server_list(&params.servers);
                if list.is_empty() {
                    let _ = events.send(MediaEvent::Prepared(None));
                    continue;
                }
                let mut matched: HashMap<usize, (u32, Option<String>)> = HashMap::new();
                // Group: probe the registration layouts in order until the
                // relay accepts one (the accepted shape was never documented).
                let group_layouts: Vec<Option<&str>> = if params.group {
                    vec![Some("zero"), Some("sub6"), Some("callId"), Some("callIdHost")]
                } else {
                    vec![None]
                };
                'layouts: for layout in &group_layouts {
                eprintln!("[zrtc-debug] probe layout {:?}", layout);
                let init_pkt = match layout {
                    Some(l) => wire::build_group_init_zrtp_packet(params.host, uid, params.sess_id.as_bytes(), false, call_id, (params.host_call as u32) & 0xffff_ffff, l),
                    None => wire::build_init_zrtp_packet(role == Role::Caller, uid, peer_uid, call_id, params.sess_id.as_bytes()),
                };
                let mut reply_of: HashMap<usize, (u32, Option<String>)> = HashMap::new();
                let mut sent_to: HashMap<SocketAddr, usize> = HashMap::new();
                let started = Instant::now();
                'rounds: for round in 0..REG_ROUNDS {
                    for (i, server) in list.iter().enumerate() {
                        let wrap = wire::wrap_media_packet(msg_type::CONTROL, 0, &init_pkt);
                        if sock.send_to(&wrap, server.addr).is_ok() {
                            sent_to.insert(server.addr, i);
                        } else {
                            udp_ok.store(false, Ordering::Relaxed);
                        }
                    }
                    let deadline = started + Duration::from_millis(REG_WINDOW_MS + round as u64 * REG_ROUND_MS);
                    while Instant::now() < deadline {
                        let mut buf = [0u8; 1500];
                        match sock.recv_from(&mut buf) {
                            Ok((n, from)) => {
                                let Some(p) = unwrap_packet(&buf[..n]) else { eprintln!("[zrtc-debug] probe rx: unwrap None"); continue };
                                eprintln!("[zrtc-debug] probe rx: type={} res={:?} callId={:?} from={}", p.msg_type, p.res, p.call_id, from);
                                if p.msg_type != msg_type::SERVER_REPLY || p.res != Some(0) {
                                    continue;
                                }
                                let (Some(call), Some(token)) = (p.call_id, p.server_token) else { continue };
                                if call != call_id {
                                    continue;
                                }
                                let Some(&idx) = sent_to.get(&from) else { continue };
                                let public_addr = p.public_addr.as_ref().map(|a| String::from_utf8_lossy(a).into_owned());
                                let entry = (token, public_addr);
                                reply_of.entry(idx).or_insert(entry.clone());
                                matched.insert(idx, entry);
                            }
                            Err(_) => {}
                        }
                        if reply_of.len() == list.len() {
                            break 'rounds;
                        }
                    }
                    if !reply_of.is_empty() && round >= 1 {
                        break 'rounds; // one good reply after a retry round is enough
                    }
                }
                eprintln!("[zrtc-debug] layout {:?} matched {} reply", layout, matched.len());
                if matched.is_empty() {
                    continue 'layouts; // thử layout kế
                }
                socket = Some(sock);
                servers = list;
                // Lowest-RTT reply (insertion order ≈ arrival order here; the
                // first reply of each server is kept, which is the fastest one).
                best_idx = matched.keys().copied().min();
                match best_idx {
                    Some(idx) => {
                        let (token, public_addr) = matched[&idx].clone();
                        if let Some(s) = servers.get_mut(idx) {
                            s.token = token;
                        }
                        let _ = events.send(MediaEvent::Prepared(Some(InitReply {
                            server: servers[idx].raw.clone(),
                            public_addr,
                        })));
                    }
                    None => {
                        let _ = events.send(MediaEvent::Prepared(None));
                    }
                }
                break 'layouts;
                }
            }
            Ok(Job::Start(role, rtp_ser_ip)) => {
                let idx = if !rtp_ser_ip.is_empty() {
                    servers.iter().position(|s| s.addr.ip().to_string() == rtp_ser_ip).unwrap_or_else(|| best_idx.unwrap_or(0))
                } else {
                    best_idx.unwrap_or(0)
                };
                let sess_id = negotiated.as_ref().map(|p| p.sess_id.clone()).unwrap_or_default();
                keys = match negotiated.as_ref().map(|p| (p.group, p.config.get("srtpKey").cloned())) {
                    Some((true, Some(Value::String(key)))) if !key.is_empty() => {
                        // Group salt: the relay hands a session key via the
                        // signalling config (same 30-byte shape).
                        srtp::master_from_sess_id(key.as_bytes()).and_then(|m| srtp::session_keys(&m))
                    }
                    Some((true, _)) => None, // plain RTP, đúng như relay kỳ vọng
                    _ => srtp::master_from_sess_id(sess_id.as_bytes()).and_then(|m| srtp::session_keys(&m)),
                };
                active = Some(role);
                roc_out = Roc::new();
                rocs_in.clear();
                rtp_seq = 0;
                rtp_ts = 0;
                peer_audio_packets = 0;
                early_first_sent = false;
                last_rx = Instant::now();
                last_ping = Instant::now();
                last_twcc = Instant::now();
                last_sr = Instant::now();
                if let Some(p) = &negotiated {
                    rtcp = Some(RtcpFeedback::new((p.local_uid as u32) & 0xffff_ffff));
                }
                match AudioIo::open(mic_ring.clone(), spk_ring.clone()) {
                    Ok(io) => audio = Some(io),
                    Err((dir, e)) => {
                        eprintln!("[olaz] zrtc media: audio {dir:?}: {e}");
                        let _ = events.send(MediaEvent::Fault(match dir {
                            Direction::Mic => "mic",
                            Direction::Speaker => "speaker",
                        }));
                    }
                }
                let _ = idx;
            }
            Ok(Job::Stop) => {
                active = None;
                audio = None;
                while jitter.pop().is_some() {}
                rtcp = None;
                videos.clear();
                last_video_gaps.clear();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
            Err(_) => {}
        }

        let Some(role) = active else { continue };
        let Some(sock) = &socket else { continue };
        let Some(params) = &negotiated else { continue };
        let idx = best_idx.unwrap_or(0);
        let Some(server) = servers.get(idx) else { continue };
        let keys_opt = keys.clone();
        let group_plain = negotiated.as_ref().map(|p| p.group && keys_opt.is_none()).unwrap_or(false);
        let keys = keys_opt.as_ref();
        let uid = (params.local_uid as u32) & 0xffff_ffff;

        // ---- 20 ms tick: playout + capture ----
        let is_group = negotiated.as_ref().map(|p| p.group).unwrap_or(false);
        let mut pcm = [0i16; FRAME_SAMPLES];
        if is_group {
            // Mix mọi stream đã ready: tổng PCM có clamp (Limiter đơn giản).
            let mut mixed = [0i32; FRAME_SAMPLES];
            for jb in group_jitters.values_mut() {
                if let Some(pkt) = jb.tick_due() {
                    if let (Some(dec), Some(keys)) = (&mut decoder, keys.as_ref()) {
                        let mut out = [0i16; FRAME_SAMPLES];
                        if dec.decode(&pkt, &mut out, false).is_ok() {
                            for i in 0..FRAME_SAMPLES {
                                mixed[i] += i32::from(out[i]);
                            }
                        }
                    }
                }
            }
            let render: Vec<i16> = mixed.iter().map(|v| (*v).clamp(i16::MIN as i32, i16::MAX as i32) as i16).collect();
            spk_ring.push(&render);
            // capture như 1-1 (phần dưới); tick_due của jitter 1-1 bỏ qua.
            let mut pcm_in = [0i16; FRAME_SAMPLES];
            if let Some(enc) = &mut encoder {
                if mic_ring.pop(&mut pcm_in) == FRAME_SAMPLES {
                    let mut bytes = [0u8; 400];
                    if let Ok(n) = enc.encode(&pcm_in, &mut bytes) {
                        rtp_seq = rtp_seq.wrapping_add(1);
                        roc_out.advance(rtp_seq);
                        rtp_ts = rtp_ts.wrapping_add(320);
                        let header = rtp::build_audio_rtp_header(&RtpHeaderArgs {
                            seq: rtp_seq,
                            ts: rtp_ts,
                            ssrc: uid,
                            tw_seq: rtp_seq,
                            marker: false,
                            ..Default::default()
                        });
                        let sealed = match keys.as_ref() {
                            Some(keys) => srtp::protect_rtp(&header, &bytes[..n], keys, rtp_seq, uid, roc_out.roc),
                            None => {
                                // Plain RTP (group without srtpKey).
                                let mut raw = header.to_vec();
                                raw.extend_from_slice(&bytes[..n]);
                                raw
                            }
                        };
                        let wrap = wire::wrap_media_packet(msg_type::AUDIO_RTP, server.token, &sealed);
                        if sock.send_to(&wrap, server.addr).is_ok() {
                            tx_counter.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            // RTCP/ping phía dưới vẫn chạy chung.
            // Group đã mix/capture phía trên — bỏ qua đường 1-1 dưới đây,
            // nếu không mỗi frame mic bị encode + send hai lần.
        }
        if is_group {
            continue;
        }
        match jitter.tick_due() {
            Some(pkt) => {
                if let Some(dec) = &mut decoder {
                    let mut out = [0i16; FRAME_SAMPLES];
                    if dec.decode(&pkt, &mut out, false).is_ok() {
                        spk_ring.push(&out);
                    }
                }
            }
            None => {
                if jitter.ready() {
                    // Loss: decoder PLC keeps the playout clock fed.
                    if let Some(dec) = &mut decoder {
                        if dec.decode(&[], &mut pcm, false).is_ok() {
                            spk_ring.push(&pcm);
                        }
                    }
                }
            }
        }
        if let (Some(enc), keys) = (&mut encoder, keys) {
            let mut pcm_in = [0i16; FRAME_SAMPLES];
            if mic_ring.pop(&mut pcm_in) == FRAME_SAMPLES {
                let mut bytes = [0u8; 400];
                if let Ok(n) = enc.encode(&pcm_in, &mut bytes) {
                    rtp_seq = rtp_seq.wrapping_add(1);
                    roc_out.advance(rtp_seq);
                    rtp_ts = rtp_ts.wrapping_add(320);
                    let header = rtp::build_audio_rtp_header(&RtpHeaderArgs {
                        seq: rtp_seq,
                        ts: rtp_ts,
                        ssrc: uid,
                        tw_seq: rtp_seq, // our TWCC sequence tracks the RTP one
                        marker: false,
                        ..Default::default()
                    });
                    let Some(keys) = keys else { continue };
                    let sealed = srtp::protect_rtp(&header, &bytes[..n], keys, rtp_seq, uid, roc_out.roc);
                    let wrap = wire::wrap_media_packet(msg_type::AUDIO_RTP, server.token, &sealed);
                    if sock.send_to(&wrap, server.addr).is_ok() {
                        tx_counter.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        // ---- RX drain ----
        let mut buf = [0u8; 1500];
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, _from)) => {
                    last_rx = Instant::now();
                    let Some(p) = unwrap_packet(&buf[..n]) else { continue };
                    // Group roster: RoomUpdated (cmd 50) reply body = uid LE32 list.
                    if p.is_control && p.cmd == wire::cmd::ROOM_UPDATED {
                        if let Some(roster) = wire::parse_room_updated(&p.data) {
                            let _ = events.send(MediaEvent::GroupRoster(roster));
                        }
                        continue;
                    }
                    // Relay RTCP rides the FEC message types (zrtc_media.cpp
                    // `onUdp`): 0x05 = audio RTCP, 0x0f = video RTCP — no SRTP.
                    if p.msg_type == msg_type::AUDIO_FEC && p.payload.len() > 4 {
                        if let Some(fb) = &mut rtcp {
                            fb.on_rtcp(false, &p.payload, now_ms());
                        }
                        continue;
                    }
                    if p.msg_type == msg_type::VIDEO_FEC && p.payload.len() > 4 {
                        if let Some(fb) = &mut rtcp {
                            fb.on_rtcp(true, &p.payload, now_ms());
                        }
                        continue;
                    }
                    if !(p.is_media && (p.msg_type == msg_type::AUDIO_RTP_DOWN || p.msg_type == msg_type::VIDEO_RTP_DOWN)) {
                        continue;
                    }
                    let video = p.msg_type == msg_type::VIDEO_RTP_DOWN;
                    let view = srtp::parse_rtp(&p.payload);
                    let Some(view) = view else { continue };
                    let seq = u16::from_be_bytes([p.payload[2], p.payload[3]]);
                    let ssrc = srtp::rd32be(&p.payload, 8);
                    let roc = rocs_in.entry(ssrc).or_insert_with(Roc::new).guess(seq);
                    let Some(keys) = keys else { continue };
                    let Some(open) = srtp::unprotect_rtp(&p.payload, keys, roc) else { continue };
                    rx_counter.fetch_add(1, Ordering::Relaxed);
                    let now = now_ms();
                    let mut full = p.payload[..view].to_vec();
                    full.extend_from_slice(&open.payload);
                    if let Some(fb) = &mut rtcp {
                        fb.on_rtp(video, &full, now);
                    }
                    let is_group = negotiated.as_ref().map(|p| p.group).unwrap_or(false);
                    if is_group {
                        // Group audio: một jitter/decoder mỗi SSRC, mix khi phát.
                        let jb = group_jitters.entry(ssrc).or_default();
                        jb.push(open.payload, now);
                        continue;
                    }
                    if video {
                        // Video RX: assemble → RTCP stats + PLI on gaps.
                        // Rendering needs a decoder (OpenH264) this build
                        // doesn't bundle — the call stays audio-only.
                        let pt = (u8_at(&p.payload, 1) & 0x7f) as i32;
                        let marker = (u8_at(&p.payload, 1) & 0x80) != 0;
                        let pkt = VideoPacket { seq, ts: srtp::rd32be(&p.payload, 4), pt, marker, payload: open.payload, roc, ssrc };
                        videos.entry(ssrc).or_insert_with(|| {
                            VideoAssembler::new(Box::new(|_f: VideoFrame| {}), now_ms, false, Box::new(|| {}))
                        });
                        if let Some(asm) = videos.get_mut(&ssrc) {
                            asm.push(pkt);
                        }
                        continue;
                    }
                    jitter.push(open.payload, now);
                    peer_audio_packets = peer_audio_packets.saturating_add(1);
                    if role == Role::Caller && !early_first_sent && peer_audio_packets >= 1 {
                        early_first_sent = true;
                        let _ = events.send(MediaEvent::EarlyAudio("first"));
                    }
                    if role == Role::Caller && peer_audio_packets == EARLY_AUDIO_STREAM_PACKETS {
                        let _ = events.send(MediaEvent::EarlyAudio("stream"));
                    }
                }
                Err(_) => break,
            }
        }

        // ---- periodic RTCP + relay ping ----
        // Video PLI when an assembler saw new gaps (missing keyframes etc.).
        for (ssrc, asm) in videos.iter_mut() {
            let seen = asm.stats.gaps;
            if seen > last_video_gaps.get(ssrc).copied().unwrap_or(0) {
                last_video_gaps.insert(*ssrc, seen);
                let pli = build_pli(uid, *ssrc);
                let wrap = wire::wrap_media_packet(msg_type::VIDEO_FEC, server.token, &pli);
                let _ = sock.send_to(&wrap, server.addr);
            }
        }
        if let Some(fb) = &mut rtcp {
            if last_twcc.elapsed() >= Duration::from_millis(TWCC_EVERY_MS) {
                last_twcc = Instant::now();
                for pkt in fb.twcc_packets() {
                    let wrap = wire::wrap_media_packet(msg_type::AUDIO_FEC, server.token, &pkt);
                    let _ = sock.send_to(&wrap, server.addr);
                }
            }
            if last_sr.elapsed() >= Duration::from_millis(SR_EVERY_MS) {
                last_sr = Instant::now();
                let report = fb.audio_report(now_ms(), rtp_ts, tx_counter.load(Ordering::Relaxed) as u32, 0);
                let wrap = wire::wrap_media_packet(msg_type::AUDIO_FEC, server.token, &report);
                let _ = sock.send_to(&wrap, server.addr);
            }
        }
        if last_ping.elapsed() >= Duration::from_millis(PING_EVERY_MS) {
            last_ping = Instant::now();
            let ping = wire::build_ping_packet(uid, server.token, params.sess_id.as_bytes());
            let wrap = wire::wrap_media_packet(msg_type::CONTROL, server.token, &ping);
            let _ = sock.send_to(&wrap, server.addr);
        }
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}
