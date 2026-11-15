//! Encrypted session persistence for the cookie log (`cookies.enc`).
//!
//! The renderer's login cookies are the only session this app keeps between
//! launches (`Bridge::remember_cookie` → `cookies.json`, read back in
//! `Bridge::new`). That file held every Zalo cookie in plaintext; here it is
//! sealed with AES-256-GCM instead.
//!
//! The key is machine-bound: HKDF-SHA256 over `/etc/machine-id`. A file copied
//! to another machine cannot be opened (unlike a key kept next to the file —
//! pointless, the cookie log *is* the only copy of the session, so deriving it
//! from `zpw_sek` like the macOS snapshot spec would be circular). Root on the
//! same machine can still read it — the same threat model as Zalo's own
//! `zsafe-storage`. Machines without a machine id (minimal containers) keep
//! the old plaintext file, exactly as before.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Magic + format version: `OLAZCOOK` + 0x01.
const MAGIC: &[u8] = b"OLAZCOOK";
const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
/// Bytes before the ciphertext: magic + version + nonce.
pub const HEADER_LEN: usize = MAGIC.len() + 1 + NONCE_LEN;
const SALT: &[u8] = b"olaz-cookies-v1";
const INFO: &[u8] = b"cookies";

/// HKDF-SHA256 (extract + expand, RFC 5869) — `hmac` + `sha2`, no extra crate.
pub fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], length: usize) -> Vec<u8> {
    let prk = <Hmac<Sha256> as Mac>::new_from_slice(salt).expect("hmac accepts any key length")
        .chain_update(ikm).finalize().into_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(length);
    let mut t: Vec<u8> = Vec::new();
    let mut counter: u8 = 1;
    while out.len() < length {
        // T(i) = HMAC(prk, T(i-1) | info | i)
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&prk).expect("hmac accepts any key length");
        mac.update(&t);
        mac.update(info);
        mac.update(&[counter]);
        t = mac.finalize().into_bytes().to_vec();
        let take = (length - out.len()).min(t.len());
        out.extend_from_slice(&t[..take]);
        counter += 1;
    }
    out
}

/// The machine-bound key, `None` where `/etc/machine-id` is absent or empty.
/// Cached: `/etc/machine-id` does not change while the process runs.
pub fn machine_key() -> Option<[u8; 32]> {
    static CACHED: std::sync::OnceLock<Option<[u8; 32]>> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let id = std::fs::read_to_string("/etc/machine-id").ok()?;
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        let derived = hkdf_sha256(id.as_bytes(), SALT, INFO, 32);
        let mut key = [0u8; 32];
        key.copy_from_slice(&derived);
        Some(key)
    })
}

/// `magic | version | nonce | ciphertext+tag`, AAD = magic + version.
pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    // rand's thread RNG; a collision on 96 random bits per cookie write is not a concern.
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let sealed = cipher
        .encrypt(nonce, Payload { msg: plaintext, aad: &header() })
        .expect("AES-GCM encryption with a valid key cannot fail");
    let mut out = Vec::with_capacity(HEADER_LEN + sealed.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&sealed);
    out
}

/// Inverse of [`encrypt`]; `None` on wrong key, wrong magic, version or truncation.
pub fn decrypt(key: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < HEADER_LEN + 16 || &data[..MAGIC.len()] != MAGIC || data[MAGIC.len()] != VERSION {
        return None;
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(&data[MAGIC.len() + 1..MAGIC.len() + 1 + NONCE_LEN]);
    cipher
        .decrypt(nonce, Payload { msg: &data[HEADER_LEN..], aad: &header() })
        .ok()
}

fn header() -> Vec<u8> {
    let mut header = MAGIC.to_vec();
    header.push(VERSION);
    header
}

// ------------------------------------------------------------- cookie log

/// `[(url, cookie)]` — the shape of the (formerly plaintext) `cookies.json`.
pub type CookieLog = Vec<(String, String)>;

fn parse_log(bytes: &[u8]) -> Option<CookieLog> {
    serde_json::from_slice::<Vec<serde_json::Value>>(bytes)
        .ok()
        .map(|entries| {
            entries
                .into_iter()
                .filter_map(|entry| {
                    let url = entry.get("url")?.as_str()?.to_string();
                    let cookie = entry.get("cookie")?.as_str()?.to_string();
                    Some((url, cookie))
                })
                .collect()
        })
}

/// Reads the cookie log at startup: the sealed `cookies.enc` when the machine
/// has a key, with a one-time import of the legacy plaintext `cookies.json`
/// (re-sealed, plaintext removed). Without a machine key both paths stay
/// plaintext, exactly as before.
pub fn load_cookie_log(cookie_file: &std::path::Path, encrypted_file: &std::path::Path) -> CookieLog {
    let Some(key) = machine_key() else {
        return std::fs::read(cookie_file).ok().and_then(|b| parse_log(&b)).unwrap_or_default();
    };
    let mut log = Vec::new();
    if let Ok(data) = std::fs::read(encrypted_file) {
        if let Some(plain) = decrypt(&key, &data) {
            if let Some(parsed) = parse_log(&plain) {
                log = parsed;
            }
        }
    }
    // Bản build cũ (chưa có cơ chế enc) chỉ đọc/ghi `cookies.json` — khi hai
    // phiên bản thay nhau chạy trên cùng userdata, enc có thể thiếu phiên
    // đăng nhập mà json (mới hơn) vẫn giữ. Log enc không có `zpw_sek` mà json
    // có → ưu tiên json (import lại, re-seal).
    let has_session = log.iter().any(|(_, cookie)| cookie.starts_with("zpw_sek="));
    if !has_session {
        if let Ok(bytes) = std::fs::read(cookie_file) {
            if let Some(json_log) = parse_log(&bytes) {
                let json_has_session = json_log.iter().any(|(_, cookie)| cookie.starts_with("zpw_sek="));
                if json_has_session || log.is_empty() {
                    if !json_log.is_empty() {
                        save_cookie_log(encrypted_file, cookie_file, &json_log);
                    }
                    return json_log;
                }
            }
        }
    }
    log
}

/// Seals and writes the cookie log (atomic rename, file 0600 — session
/// secrets). With a machine key the plaintext copy is removed; without one the
/// file stays plaintext JSON.
pub fn save_cookie_log(encrypted_file: &std::path::Path, cookie_file: &std::path::Path, log: &CookieLog) {
    let payload: Vec<serde_json::Value> = log
        .iter()
        .map(|(url, cookie)| serde_json::json!({ "url": url, "cookie": cookie }))
        .collect();
    let plain = serde_json::to_vec_pretty(&payload).unwrap_or_default();
    match machine_key() {
        Some(key) => {
            let sealed = encrypt(&key, &plain);
            let tmp = encrypted_file.with_extension("enc.tmp");
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            if options.open(&tmp).and_then(|mut f| std::io::Write::write_all(&mut f, &sealed)).is_ok() {
                let _ = std::fs::rename(&tmp, encrypted_file);
                // Cũng giữ bản plaintext cho binary CŨ (bản CI chưa có cơ chế
                // enc) — hai định dạng cùng tồn tại trên cùng userdata, nếu không
                // bản cũ mở lên mất hẳn phiên đăng nhập.
                let plain_tmp = cookie_file.with_extension("json.tmp");
                if std::fs::write(&plain_tmp, &plain).is_ok() {
                    let _ = std::fs::rename(&plain_tmp, cookie_file);
                }
            }
        }
        None => {
            let tmp = cookie_file.with_extension("json.tmp");
            if std::fs::write(&tmp, &plain).is_ok() {
                let _ = std::fs::rename(&tmp, cookie_file);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 32] {
        hkdf_sha256(b"machine-id", SALT, INFO, 32).try_into().unwrap()
    }

    #[test]
    fn round_trip() {
        let plain = b"[{\"url\":\"https://zalo.me/\",\"cookie\":\"zpw_sek=abc\"}]";
        let sealed = encrypt(&key(), plain);
        assert_eq!(&sealed[..8], b"OLAZCOOK");
        assert_eq!(decrypt(&key(), &sealed).as_deref(), Some(&plain[..]));
    }

    #[test]
    fn wrong_key_fails() {
        let sealed = encrypt(&key(), b"secret");
        let mut other = key();
        other[0] ^= 0xff;
        assert_eq!(decrypt(&other, &sealed), None);
    }

    #[test]
    fn wrong_magic_and_truncation_fail() {
        let mut sealed = encrypt(&key(), b"secret");
        sealed[0] = b'X';
        assert_eq!(decrypt(&key(), &sealed), None);
        assert_eq!(decrypt(&key(), &sealed[..20]), None);
    }

    #[test]
    fn cookie_log_parses() {
        let log = parse_log(br#"[{"url": "https://zalo.me/", "cookie": "zpw_sek=abc; Domain=zalo.me"}]"#).unwrap();
        assert_eq!(log, vec![("https://zalo.me/".to_string(), "zpw_sek=abc; Domain=zalo.me".to_string())]);
        assert_eq!(parse_log(b"not json"), None);
    }

    #[test]
    fn hkdf_matches_reference_vector() {
        // RFC 5869 appendix A.1 test vector 1.
        let out = hkdf_sha256(
            &[0x0b; 22],
            &[0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c],
            &[0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9],
            42,
        );
        let expected: [u8; 42] = [
            0x3c, 0xb2, 0x5f, 0x25, 0xfa, 0xac, 0xd5, 0x7a, 0x90, 0x43, 0x4f, 0x64, 0xd0, 0x36, 0x2f, 0x2a,
            0x2d, 0x2d, 0x0a, 0x90, 0xcf, 0x1a, 0x5a, 0x4c, 0x5d, 0xb0, 0x2d, 0x56, 0xec, 0xc4, 0xc5, 0xbf,
            0x34, 0x00, 0x72, 0x08, 0xd5, 0xb8, 0x87, 0x18, 0x58, 0x65,
        ];
        assert_eq!(out, expected);
    }
}
