//! SRTP as Zalo's zrtc relays expect it — a byte-exact port of the Zalos
//! client's `engines/zcall-cpp/engine/src/wire/srtp.{h,cpp}` (BSD-3-Clause).
//!
//! The master key is **not** negotiated: it is the first 30 UTF-8 bytes of the
//! server-issued `sessId` (16-byte AES-128 key + 14-byte salt) — hop-by-hop
//! transport encryption, the relay knows the session id. Group calls derive
//! the salt from `zrtc_config.srtpKey` instead (same 30-byte shape); with no
//! key the call runs plain RTP.
//!
//! Packet: AES-128-CTR over the payload, HMAC-SHA1-80 appended, ROC mixed into
//! the MAC input; IV = salt ⊕ {ssrc BE32 @4, (roc·2¹⁶+seq)·2¹⁶ @8..15}.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use sha1::Sha1;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// Derived session keys: 16-byte encryption, 20-byte auth, 14-byte salt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrtpKeys {
    pub enc: [u8; 16],
    pub auth: [u8; 20],
    pub salt: [u8; 14],
}

fn rd16be(buf: &[u8], at: usize) -> usize {
    u16::from_be_bytes([buf[at], buf[at + 1]]) as usize
}

pub fn rd32be(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

fn wr32be(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

/// AES-CM keystream expansion: AES-128-CTR over `n` zero bytes, counter block
/// starting at `iv16` (bytes 14/15 zeroed — the KDF counter position).
fn aes_cm_expand(key: &[u8; 16], iv16: &[u8; 16], n: usize) -> Vec<u8> {
    let mut iv = *iv16;
    iv[14] = 0;
    iv[15] = 0;
    let mut cipher = Aes128Ctr::new(key.into(), GenericArray::from_slice(&iv));
    let mut out = vec![0u8; n];
    cipher.apply_keystream(&mut out);
    out
}

/// SRTP KDF: the session salt XORed with the label at byte 7, then expanded.
fn kdf(master_key: &[u8; 16], master_salt: &[u8; 14], label: u8, out_len: usize) -> Vec<u8> {
    let mut x = [0u8; 16];
    x[..14].copy_from_slice(master_salt);
    x[7] ^= label;
    aes_cm_expand(master_key, &x, out_len)
}

/// The master material: first 30 bytes of the UTF-8 `sessId` (or the group
/// `srtpKey`). `None` when shorter — then the call runs plain RTP.
pub fn master_from_sess_id(sess_id_utf8: &[u8]) -> Option<Vec<u8>> {
    if sess_id_utf8.len() < 30 {
        return None;
    }
    Some(sess_id_utf8[..30].to_vec())
}

/// The three session keys from 30 master bytes (16 key + 14 salt).
pub fn session_keys(master30: &[u8]) -> Option<SrtpKeys> {
    if master30.len() < 30 {
        return None;
    }
    let mut mk = [0u8; 16];
    mk.copy_from_slice(&master30[..16]);
    let mut ms = [0u8; 14];
    ms.copy_from_slice(&master30[16..30]);
    let mut enc = [0u8; 16];
    enc.copy_from_slice(&kdf(&mk, &ms, 0x00, 16));
    let mut auth = [0u8; 20];
    auth.copy_from_slice(&kdf(&mk, &ms, 0x01, 20));
    let mut salt = [0u8; 14];
    salt.copy_from_slice(&kdf(&mk, &ms, 0x02, 14));
    Some(SrtpKeys { enc, auth, salt })
}

/// Per-packet IV: session salt ⊕ {ssrc BE32 @4, (roc·2¹⁶+seq)·2¹⁶ @8..15}.
pub fn packet_iv(session_salt: &[u8; 14], ssrc: u32, seq: u16, roc: u32) -> [u8; 16] {
    let idx = u64::from(roc) * 0x1_0000 + u64::from(seq);
    let shifted = idx * 0x1_0000;
    let mut iv = [0u8; 16];
    wr32be(&mut iv, 4, ssrc);
    wr32be(&mut iv, 8, ((shifted >> 32) & 0xffff_ffff) as u32);
    wr32be(&mut iv, 12, (shifted & 0xffff_ffff) as u32);
    for i in 0..14 {
        iv[i] ^= session_salt[i];
    }
    iv
}

fn hmac10(key: &[u8; 20], data: &[u8]) -> [u8; 10] {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    let out = mac.finalize().into_bytes();
    let mut tag = [0u8; 10];
    tag.copy_from_slice(&out[..10]);
    tag
}

fn mac_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Where the RTP header ends (fixed header + CSRCs + one extension block).
pub fn parse_rtp(buf: &[u8]) -> Option<usize> {
    if buf.len() < 12 {
        return None;
    }
    let x = (i32::from(buf[0]) >> 4) & 1;
    let mut off = 12 + 4 * usize::from(buf[0] & 0x0f);
    if x == 1 {
        let rest = buf.get(off..off + 4)?;
        off += 4 + 4 * rd16be(rest, 2);
    }
    if off > buf.len() {
        return None;
    }
    Some(off)
}

pub struct Unprotected {
    pub seq: u16,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

/// Verifies the 10-byte tag and decrypts; `None` on any mismatch/truncation.
pub fn unprotect_rtp(rtp: &[u8], keys: &SrtpKeys, roc: u32) -> Option<Unprotected> {
    let header_len = parse_rtp(rtp)?;
    let payload_len = rtp.len().checked_sub(header_len)?;
    if payload_len < 10 {
        return None;
    }
    let body_len = payload_len - 10;
    let mut authed = Vec::with_capacity(header_len + body_len + 4);
    authed.extend_from_slice(&rtp[..header_len + body_len]);
    authed.extend_from_slice(&roc.to_be_bytes());
    let mac = hmac10(&keys.auth, &authed);
    if !mac_eq(&mac, &rtp[rtp.len() - 10..]) {
        return None;
    }
    let seq = u16::from_be_bytes([rtp[2], rtp[3]]);
    let ssrc = rd32be(rtp, 8);
    let iv = packet_iv(&keys.salt, ssrc, seq, roc);
    let mut payload = rtp[header_len..header_len + body_len].to_vec();
    Aes128Ctr::new((&keys.enc).into(), GenericArray::from_slice(&iv)).apply_keystream(&mut payload);
    Some(Unprotected { seq, ssrc, payload })
}

/// Encrypts `payload` under `header_and_ext` (fixed header + extensions) and
/// appends the 10-byte tag; returns the wire-ready SRTP packet.
pub fn protect_rtp(header_and_ext: &[u8], payload: &[u8], keys: &SrtpKeys, seq: u16, ssrc: u32, roc: u32) -> Vec<u8> {
    let iv = packet_iv(&keys.salt, ssrc, seq, roc);
    let mut cipher = Aes128Ctr::new((&keys.enc).into(), GenericArray::from_slice(&iv));
    let mut out = Vec::with_capacity(header_and_ext.len() + payload.len() + 10);
    out.extend_from_slice(header_and_ext);
    let start = out.len();
    out.extend_from_slice(payload);
    cipher.apply_keystream(&mut out[start..]);
    out.extend_from_slice(&roc.to_be_bytes());
    let mac = hmac10(&keys.auth, &out);
    out.truncate(out.len() - 4);
    out.extend_from_slice(&mac);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zrtc::wire::unhex;
    use serde_json::Value;

    const VECTORS: &str = include_str!("testdata/srtp.json");

    fn keys() -> SrtpKeys {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let master = master_from_sess_id(v["sessId"].as_str().unwrap().as_bytes()).unwrap();
        let keys = session_keys(&master).unwrap();
        assert_eq!(hex(&keys.enc), v["enc"]);
        assert_eq!(hex(&keys.auth), v["auth"]);
        assert_eq!(hex(&keys.salt), v["salt"]);
        keys
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn kdf_and_ivs_match_vectors() {
        let keys = keys();
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        for e in v["ivs"].as_array().unwrap() {
            let iv = packet_iv(&keys.salt, e["ssrc"].as_u64().unwrap() as u32, e["seq"].as_u64().unwrap() as u16, e["roc"].as_u64().unwrap() as u32);
            assert_eq!(hex(&iv), e["iv"], "ssrc {}", e["ssrc"]);
        }
    }

    #[test]
    fn protect_and_unprotect_match_vectors() {
        let keys = keys();
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        for e in v["packets"].as_array().unwrap() {
            let out = protect_rtp(
                &unhex(e["hdr"].as_str().unwrap()),
                &unhex(e["payload"].as_str().unwrap()),
                &keys,
                e["seq"].as_u64().unwrap() as u16,
                e["ssrc"].as_u64().unwrap() as u32,
                e["roc"].as_u64().unwrap() as u32,
            );
            assert_eq!(hex(&out), e["out"]);
            let back = unprotect_rtp(&out, &keys, e["roc"].as_u64().unwrap() as u32).expect("unprotect");
            assert_eq!(hex(&back.payload), e["unprotected"]);
            assert_eq!(back.seq, e["seq"].as_u64().unwrap() as u16);
            assert_eq!(back.ssrc, e["ssrc"].as_u64().unwrap() as u32);
        }
    }

    #[test]
    fn tampered_packet_fails() {
        let keys = keys();
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(
            unprotect_rtp(&unhex(v["tampered"].as_str().unwrap()), &keys, 0).is_some(),
            v["tamperedOk"].as_bool().unwrap()
        );
    }

    #[test]
    fn short_master_is_rejected() {
        assert!(master_from_sess_id(&[b'a'; 29]).is_none());
        assert!(master_from_sess_id(&[b'a'; 30]).is_some());
        assert!(session_keys(&[b'a'; 29]).is_none());
    }

    #[test]
    fn round_trip_random() {
        // Beyond the vectors: payload of odd length, extension header, roc > 0.
        let master = master_from_sess_id(&sess_id()).unwrap();
        let keys = session_keys(&master).unwrap();
        let header: &[u8] = &[
            0x90, 0x70, 0x12, 0x34, 0x00, 0x01, 0xd1, 0xc0, 0xde, 0xad, 0xbe, 0xef, // fixed 12, X=1
            0xbe, 0xde, 0x00, 0x00, // extension: profile + 0 words
        ];
        let payload: Vec<u8> = (0..33u8).collect();
        let sealed = protect_rtp(header, &payload, &keys, 0x1234, 0xdeadbeef, 7);
        let opened = unprotect_rtp(&sealed, &keys, 7).expect("round trip");
        assert_eq!(opened.payload, payload);
        assert!(unprotect_rtp(&sealed, &keys, 8).is_none(), "wrong ROC must fail the tag");
    }

    fn sess_id() -> Vec<u8> {
        b"sess-0123456789abcdefghijklmnopqrstuv".to_vec()
    }
}
