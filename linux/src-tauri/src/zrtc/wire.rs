//! The ZRTC relay wire protocol — the UDP framing Zalo's media edge expects.
//!
//! Ported from the Zalos client's `engines/zcall-cpp/engine/src/wire/zrtp_packet.{h,cpp}`
//! (BSD-3-Clause) so every packet matches the server byte-for-byte; the golden vectors in
//! `testdata/zrtp.json` pin the layout. Two caveats preserved from the original:
//!
//! * "ZRTP" is a misnomer — `InitZRTP` is a relay *registration* handshake, no key agreement.
//! * Endianness is mixed: control headers and lengths are little-endian, RTP fields are
//!   big-endian, and the P2P bind carries one big-endian field (`index`) — see `build_p2p_bind`.

use serde_json::{json, Value};

pub mod msg_type {
    pub const CONTROL: i32 = 0x01;
    pub const SERVER_REPLY: i32 = 0x02;
    pub const AUDIO_RTP: i32 = 0x03;
    pub const AUDIO_RTP_DOWN: i32 = 0x04;
    pub const AUDIO_FEC: i32 = 0x05;
    pub const AUDIO_RTCP: i32 = 0x07;
    pub const VIDEO_RTP: i32 = 0x0d;
    pub const VIDEO_DOWN: i32 = 0x0e;
    pub const VIDEO_FEC: i32 = 0x0f;
    pub const P2P_EXT: i32 = 0x7f;
}

pub mod cmd {
    pub const PING: i32 = 2;
    pub const END_CALL: i32 = 3;
    pub const INIT_CALL_CALLER: i32 = 11;
    pub const INIT_CALL_CALLEE: i32 = 12;
    pub const ECHO: i32 = 5;
    pub const CHANGE_ADDRESS: i32 = 14;
    pub const REQ_FORWARD: i32 = 32;
    pub const GROUP_MESSAGE: i32 = 33;
    pub const ROOM_UPDATED: i32 = 50;
    pub const INIT_ZAVI_PING: i32 = 0x33;
}

pub mod p2p {
    pub const HEADER_LEN: usize = 9;
    pub const BIND: u8 = 1;
    pub const BIND_PEER: u8 = 2;
    pub const BIND_ANSWER: u8 = 3;
    pub const KEEPALIVE: u8 = 4;
    pub const AUDIO: u8 = 6;
    pub const AUDIO_RTCP: u8 = 7;
    pub const VIDEO: u8 = 8;
    pub const VIDEO_RTCP: u8 = 9;
}

pub const MAGIC_BYTE: u8 = 0x7e;
pub const CONTROL_HEADER_LEN: usize = 21;
pub const MEDIA_HEADER_LEN: usize = 5;

/// Group-init registration layouts, tried in this order against the relay
/// (the server accepted shape was never documented — the engine probes).
pub const GROUP_INIT_LAYOUTS: [&str; 4] = ["zero", "sub6", "callId", "callIdHost"];

pub fn is_media_type(t: i32) -> bool {
    matches!(t, 0x03 | 0x04 | 0x05 | 0x07 | 0x0d | 0x0e | 0x0f)
}

// ------------------------------------------------------------------ little-endian writers

fn wr16le(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&(value as u16).to_le_bytes());
}

fn wr32le(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

/// `u16String`: LE16 length followed by the raw bytes — the string shape every
/// control body uses for `sessId` and friends.
pub fn u16_string(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    wr16le(&mut out, bytes.len() as u32);
    out.extend_from_slice(bytes);
    out
}

fn rd16le(buf: &[u8], at: usize) -> usize {
    u16::from_le_bytes([buf[at], buf[at + 1]]) as usize
}

fn rd32le(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

/// A length-prefixed slice that clamps instead of panicking: inbound datagrams
/// come from the network, and a malformed one must parse to "no field", never
/// kill the engine thread (the C++ original relies on `QByteArray::mid`).
fn slice_opt(buf: &[u8], start: usize, len: usize) -> Option<&[u8]> {
    buf.get(start..start.saturating_add(len))
}

// ------------------------------------------------------------------ builders

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlHeader {
    pub msg_type: i32,
    pub magic: u8,
    pub field02: u32,
    pub seq: u32,
    pub uid: u32,
    pub token: u32,
    pub cmd: i32,
    pub sub_cmd: i32,
}

impl Default for ControlHeader {
    fn default() -> Self {
        Self { msg_type: msg_type::CONTROL, magic: MAGIC_BYTE, field02: 0, seq: 0, uid: 0, token: 0, cmd: 0, sub_cmd: 0 }
    }
}

pub fn wrap_media_packet(msg_type: i32, token: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MEDIA_HEADER_LEN + payload.len());
    out.push(msg_type as u8);
    wr32le(&mut out, token);
    out.extend_from_slice(payload);
    out
}

pub fn build_control_header(h: &ControlHeader) -> Vec<u8> {
    let mut buf = vec![0u8; CONTROL_HEADER_LEN];
    buf[0] = h.msg_type as u8;
    buf[1] = h.magic;
    buf[2..6].copy_from_slice(&h.field02.to_le_bytes());
    buf[6..10].copy_from_slice(&h.seq.to_le_bytes());
    buf[10..14].copy_from_slice(&h.uid.to_le_bytes());
    buf[14..18].copy_from_slice(&h.token.to_le_bytes());
    buf[18..20].copy_from_slice(&(h.cmd as u16).to_le_bytes());
    buf[20] = (h.sub_cmd & 0xff) as u8;
    buf
}

fn header(magic: u8, uid: u32, token: u32, cmd: i32, sub_cmd: i32, seq: u32) -> ControlHeader {
    ControlHeader { msg_type: msg_type::CONTROL, magic, field02: 0, seq, uid, token, cmd, sub_cmd }
}

/// Relay registration: cmd 11 (caller) / 12 (callee), sub 2, body
/// `callId LE32 | peerUid LE32 | u16String(sessId) [| u16String(extra)]`.
pub fn build_init_zrtp_packet(caller: bool, uid: u32, peer_uid: u32, call_id: u32, sess_id: &[u8]) -> Vec<u8> {
    build_init_zrtp_packet_with(caller, uid, peer_uid, call_id, sess_id, &[], 0x02)
}

pub fn build_init_zrtp_packet_with(
    caller: bool, uid: u32, peer_uid: u32, call_id: u32, sess_id: &[u8], extra: &[u8], sub_cmd: i32,
) -> Vec<u8> {
    let mut out = build_control_header(&header(
        MAGIC_BYTE,
        uid,
        0,
        if caller { cmd::INIT_CALL_CALLER } else { cmd::INIT_CALL_CALLEE },
        sub_cmd,
        0,
    ));
    wr32le(&mut out, call_id);
    wr32le(&mut out, peer_uid);
    out.extend_from_slice(&u16_string(sess_id));
    if !extra.is_empty() {
        out.extend_from_slice(&u16_string(extra));
    }
    out
}

/// `\x00\x05[videoOn][initZrtp]` — the group "extra" string.
pub fn group_extra(video_on: bool, init_zrtp: bool) -> Vec<u8> {
    let mut b = vec![0u8, 5, u8::from(video_on)];
    if init_zrtp {
        b.push(1);
    }
    b
}

pub fn build_group_init_zrtp_packet(
    host: bool, uid: u32, sess_id: &[u8], video_on: bool, call_id: u32, host_call: u32, layout: &str,
) -> Vec<u8> {
    let sub6 = layout == "sub6";
    let mut out = build_control_header(&header(
        MAGIC_BYTE,
        uid,
        0,
        if host { cmd::INIT_CALL_CALLER } else { cmd::INIT_CALL_CALLEE },
        if sub6 { 0x06 } else { 0x04 },
        0,
    ));
    let extra = u16_string(&group_extra(video_on, true));
    if sub6 {
        wr32le(&mut out, call_id);
        out.push(u8::from(host));
        out.push(5);
        out.push(0);
        out.extend_from_slice(&u16_string(sess_id));
        out.extend_from_slice(&extra);
        return out;
    }
    let a = if layout == "zero" { 0 } else { call_id };
    let b = if layout == "callIdHost" { host_call } else { 0 };
    wr32le(&mut out, a);
    wr32le(&mut out, b);
    out.extend_from_slice(&u16_string(sess_id));
    out.extend_from_slice(&extra);
    out
}

pub fn build_share_init_zrtp_packet(uid: u32, sess_id: &[u8]) -> Vec<u8> {
    let mut out = build_control_header(&header(MAGIC_BYTE, uid, 0, cmd::INIT_CALL_CALLEE, 0x04, 0));
    wr32le(&mut out, 0);
    wr32le(&mut out, 0);
    out.extend_from_slice(&u16_string(sess_id));
    out.extend_from_slice(&u16_string(&[0x00, 0xff, 0x01, 0x00]));
    out
}

pub fn build_share_zavi_ping_packet(uid: u32, token: u32, sess_id: &[u8]) -> Vec<u8> {
    let mut out = build_control_header(&header(MAGIC_BYTE, uid, token, cmd::INIT_ZAVI_PING, 0x01, 0));
    wr32le(&mut out, 0);
    wr32le(&mut out, 0);
    out.extend_from_slice(&u16_string(sess_id));
    out
}

pub fn build_zavi_ping_packet(uid: u32, token: u32, sess_id: &[u8], video_on: bool) -> Vec<u8> {
    let mut out = build_share_zavi_ping_packet(uid, token, sess_id);
    out.extend_from_slice(&u16_string(&group_extra(video_on, false)));
    out
}

pub fn parse_room_updated(data: &[u8]) -> Option<Vec<u32>> {
    if data.len() % 4 != 0 {
        return None;
    }
    Some((0..data.len()).step_by(4).map(|o| rd32le(data, o)).collect())
}

pub fn build_ping_packet(uid: u32, token: u32, sess_id: &[u8]) -> Vec<u8> {
    let mut out = build_control_header(&header(0x01, uid, token, cmd::PING, 0x01, 0));
    out.extend_from_slice(&u16_string(sess_id));
    out
}

pub fn build_end_call_packet(uid: u32, token: u32, call_id: u32) -> Vec<u8> {
    let mut out = build_control_header(&header(MAGIC_BYTE, uid, token, cmd::END_CALL, 0, 0));
    wr32le(&mut out, call_id);
    out
}

pub fn build_req_forward_packet(uid: u32, token: u32, seq: u32, sub_cmd: i32, payload: &[u8]) -> Vec<u8> {
    let mut out = build_control_header(&header(0x01, uid, token, cmd::REQ_FORWARD, sub_cmd, seq));
    out.extend_from_slice(payload);
    out
}

/// ReqForward sub 10 body: `[20, a, 1, 1, rxPerSecond, b5]`, all LE32, 24 bytes.
pub fn req_forward_status(rx_per_second: u32, a: u32, b5: u32) -> Vec<u8> {
    let mut b = vec![0u8; 24];
    let v = [20u32, a, 1, 1, rx_per_second, b5];
    for (i, value) in v.iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    b
}

pub struct VidQualEntry {
    pub uid: u32,
    pub quality_id: u8,
}

pub fn build_group_vid_qual_req(uid: u32, token: u32, seq: u32, entries: &[VidQualEntry]) -> Vec<u8> {
    let mut body = vec![0u8; 1 + entries.len() * 5];
    body[0] = entries.len() as u8;
    for (i, entry) in entries.iter().enumerate() {
        let o = 1 + i * 5;
        body[o..o + 4].copy_from_slice(&entry.uid.to_le_bytes());
        body[o + 4] = entry.quality_id;
    }
    let mut out = build_control_header(&header(0x01, uid, token, cmd::REQ_FORWARD, 0x0d, seq));
    out.extend_from_slice(&u16_string(&body));
    out
}

pub struct VidLayer {
    pub id: u8,
    pub a: u32,
    pub b: u16,
    pub c: u16,
}

pub fn build_group_vid_qual_config(uid: u32, token: u32, seq: u32, layers: &[VidLayer]) -> Vec<u8> {
    let mut body = vec![0u8; 1 + layers.len() * 9];
    body[0] = layers.len() as u8;
    for (i, layer) in layers.iter().enumerate() {
        let o = 1 + i * 9;
        body[o] = layer.id;
        body[o + 1..o + 5].copy_from_slice(&layer.a.to_le_bytes());
        body[o + 5..o + 7].copy_from_slice(&layer.b.to_le_bytes());
        body[o + 7..o + 9].copy_from_slice(&layer.c.to_le_bytes());
    }
    let mut out = build_control_header(&header(0x01, uid, token, cmd::REQ_FORWARD, 0x0c, seq));
    out.extend_from_slice(&u16_string(&body));
    out
}

pub fn build_group_sync_packet(uid: u32, token: u32, seq: u32, sub_cmd: i32) -> Vec<u8> {
    build_control_header(&header(0x01, uid, token, cmd::GROUP_MESSAGE, sub_cmd, seq))
}

// ------------------------------------------------------------------ P2P framing (0x7f)

fn p2p_header(callee: bool, call_id: u32, kind: u8) -> Vec<u8> {
    let mut h = vec![0u8; p2p::HEADER_LEN];
    h[0] = msg_type::P2P_EXT as u8;
    h[2] = u8::from(callee);
    h[3..7].copy_from_slice(&call_id.to_le_bytes());
    h[7] = kind;
    h
}

pub fn build_p2p_bind(callee: bool, call_id: u32, seq: u32, index: u32) -> Vec<u8> {
    let mut b = vec![0u8; 6];
    b[0..2].copy_from_slice(&((seq & 0xffff) as u16).to_le_bytes());
    // The one big-endian field in the whole control plane.
    b[2..6].copy_from_slice(&index.to_be_bytes());
    let mut out = p2p_header(callee, call_id, p2p::BIND);
    out.extend_from_slice(&b);
    out
}

pub fn build_p2p_bind_answer(callee: bool, call_id: u32, request: &[u8], peer_addr: &[u8]) -> Vec<u8> {
    let mut out = p2p_header(callee, call_id, p2p::BIND_ANSWER);
    out.extend_from_slice(&request[9..15]);
    wr32le(&mut out, peer_addr.len() as u32);
    out.extend_from_slice(peer_addr);
    out
}

/// The keepalive pong: the ping echoed back with our callee flag and kind 2.
pub fn build_p2p_pong(callee: bool, ping: &[u8]) -> Vec<u8> {
    let mut pong = ping.to_vec();
    pong[2] = u8::from(callee);
    pong[9] = 2;
    pong
}

pub fn wrap_p2p_audio(callee: bool, call_id: u32, rtp: &[u8]) -> Vec<u8> {
    let mut out = p2p_header(callee, call_id, p2p::AUDIO);
    out.extend_from_slice(rtp);
    out
}

pub fn wrap_p2p_video(callee: bool, call_id: u32, rtp: &[u8]) -> Vec<u8> {
    let mut out = p2p_header(callee, call_id, p2p::VIDEO);
    out.extend_from_slice(rtp);
    out
}

pub fn wrap_p2p_rtcp(callee: bool, call_id: u32, video: bool, rtcp: &[u8]) -> Vec<u8> {
    let mut out = p2p_header(callee, call_id, if video { p2p::VIDEO_RTCP } else { p2p::AUDIO_RTCP });
    out.extend_from_slice(rtcp);
    out
}

// ------------------------------------------------------------------ parse

/// One decoded inbound datagram. `Option` fields are set only for the body
/// shapes the engine cares about (init reply, ping, end call, …).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Unwrapped {
    pub is_media: bool,
    pub msg_type: i32,
    pub truncated: bool,
    pub token: u32,
    pub payload: Vec<u8>,
    pub is_control: bool,
    pub magic: u8,
    pub field02: u32,
    pub seq: u32,
    pub uid: u32,
    pub cmd: i32,
    pub sub_cmd: i32,
    pub data: Vec<u8>,
    pub res: Option<u32>,
    pub call_id: Option<u32>,
    pub server_token: Option<u32>,
    pub public_addr: Option<Vec<u8>>,
    pub peer_uid: Option<u32>,
    pub sess_id: Option<Vec<u8>>,
    pub extra: Option<Vec<u8>>,
    /// P2P datagram fields (`b1` byte 1, `b2` = callee flag, `field38` = callId LE32,
    /// `cmd` = kind at byte 7).
    pub b1: u8,
    pub b2: u8,
    pub field38: u32,
    pub raw: Vec<u8>,
}

pub fn unwrap_packet(buf: &[u8]) -> Option<Unwrapped> {
    if buf.is_empty() {
        return None;
    }
    let mut p = Unwrapped { msg_type: i32::from(buf[0]), ..Default::default() };
    if p.msg_type == msg_type::AUDIO_RTP_DOWN || p.msg_type == msg_type::VIDEO_DOWN {
        p.is_media = true;
        p.payload = buf[1..].to_vec();
        return Some(p);
    }
    if is_media_type(p.msg_type) {
        p.is_media = true;
        if buf.len() < MEDIA_HEADER_LEN {
            p.truncated = true;
            p.raw = buf.to_vec();
            return Some(p);
        }
        p.token = rd32le(buf, 1);
        p.payload = buf[5..].to_vec();
        return Some(p);
    }
    if (p.msg_type == msg_type::CONTROL || p.msg_type == msg_type::SERVER_REPLY) && buf.len() >= CONTROL_HEADER_LEN {
        p.is_control = true;
        p.magic = buf[1];
        p.field02 = rd32le(buf, 2);
        p.seq = rd32le(buf, 6);
        p.uid = rd32le(buf, 10);
        p.token = rd32le(buf, 14);
        p.cmd = rd16le(buf, 18) as i32;
        p.sub_cmd = i32::from(buf[20]);
        p.data = buf[CONTROL_HEADER_LEN..].to_vec();
        let data = &p.data;
        let init_cmd = p.cmd == cmd::INIT_CALL_CALLER || p.cmd == cmd::INIT_CALL_CALLEE;
        if init_cmd && p.msg_type == msg_type::SERVER_REPLY && data.len() >= 4 {
            p.res = Some(rd32le(data, 0));
            if p.res == Some(0) && data.len() >= 12 {
                p.call_id = Some(rd32le(data, 4));
                p.server_token = Some(rd32le(data, 8));
                if data.len() >= 14 {
                    let n = rd16le(data, 12);
                    p.public_addr = slice_opt(data, 14, n).map(<[u8]>::to_vec);
                }
            }
        } else if init_cmd && data.len() >= 10 {
            p.call_id = Some(rd32le(data, 0));
            p.peer_uid = Some(rd32le(data, 4));
            let n1 = rd16le(data, 8);
            p.sess_id = slice_opt(data, 10, n1).map(<[u8]>::to_vec);
            let off = 10 + n1;
            if let Some(len_bytes) = slice_opt(data, off, 2) {
                p.extra = slice_opt(data, off + 2, rd16le(len_bytes, 0)).map(<[u8]>::to_vec);
            }
        } else if p.cmd == cmd::PING && data.len() >= 2 {
            p.sess_id = slice_opt(data, 2, rd16le(data, 0)).map(<[u8]>::to_vec);
        } else if p.cmd == cmd::END_CALL && data.len() >= 4 {
            p.call_id = Some(rd32le(data, 0));
        }
        return Some(p);
    }
    if p.msg_type == msg_type::P2P_EXT && buf.len() >= p2p::HEADER_LEN {
        p.b1 = buf[1];
        p.b2 = buf[2];
        p.field38 = rd32le(buf, 3);
        p.cmd = rd16le(buf, 7) as i32;
        p.data = buf[p2p::HEADER_LEN..].to_vec();
        return Some(p);
    }
    p.raw = buf.to_vec();
    Some(p)
}

/// The golden-vector shape of [`Unwrapped`] (same fields the C++ test compares).
pub fn unwrapped_json(p: &Unwrapped) -> Value {
    let hex_obj = |b: &[u8]| json!({ "hex": hex(b) });
    let mut o = json!({ "isMedia": p.is_media, "msgType": p.msg_type });
    if p.is_media {
        if p.truncated {
            o["truncated"] = json!(true);
            o["raw"] = hex_obj(&p.raw);
            return o;
        }
        if p.msg_type != msg_type::AUDIO_RTP_DOWN && p.msg_type != msg_type::VIDEO_DOWN {
            o["token"] = json!(p.token);
        }
        o["payload"] = hex_obj(&p.payload);
        return o;
    }
    if p.is_control {
        o["magic"] = json!(p.magic);
        o["field02"] = json!(p.field02);
        o["seq"] = json!(p.seq);
        o["uid"] = json!(p.uid);
        o["token"] = json!(p.token);
        o["cmd"] = json!(p.cmd);
        o["subCmd"] = json!(p.sub_cmd);
        o["data"] = hex_obj(&p.data);
        if let Some(res) = p.res {
            o["res"] = json!(res);
        }
        if let Some(call_id) = p.call_id {
            o["callId"] = json!(call_id);
        }
        if let Some(token) = p.server_token {
            o["serverToken"] = json!(token);
        }
        if let Some(addr) = &p.public_addr {
            o["publicAddr"] = json!(String::from_utf8_lossy(addr));
        }
        if let Some(peer) = p.peer_uid {
            o["peerUid"] = json!(peer);
        }
        if let Some(sess) = &p.sess_id {
            o["sessId"] = hex_obj(sess);
        }
        if let Some(extra) = &p.extra {
            o["extra"] = hex_obj(extra);
        }
        return o;
    }
    if p.msg_type == msg_type::P2P_EXT && p.raw.is_empty() {
        o["b1"] = json!(p.b1);
        o["b2"] = json!(p.b2);
        o["field38"] = json!(p.field38);
        o["cmd"] = json!(p.cmd);
        o["data"] = hex_obj(&p.data);
        return o;
    }
    o["raw"] = hex_obj(&p.raw);
    o
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}
