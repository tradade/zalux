//! Network proxy for the official renderer: the `znet` message handler.
//!
//! `HostScripts/network-shim.js` replaces XHR/fetch/WebSocket in the page and
//! sends `{op:'http'|'ws.open'|'ws.send'|'ws.close', …}` here. Same contract as
//! `Sources/Olaz/ZaloRuntime/NetworkProxy.swift`, reimplemented on reqwest
//! (blocking) + tungstenite so the renderer never talks to the network itself.
//!
//! TLS and HTTP/2 are Chromium 108's, the official client's: every request goes
//! through the repository's Go `FingerprintHelper` sidecar (see `fingerprint.rs`),
//! which is what the macOS build does too. rustls is the fallback when that
//! helper cannot start.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Mutex, OnceLock, mpsc};

use base64::Engine;
use serde_json::{json, Map, Value};

use crate::host::{AppContext, Events};

/// Chromium 108 header set, captured in `reanalysis/wire-fingerprint.md`.
const ACCEPT_LANGUAGE: &str = "en-US";
const SEC_CH_UA: &str = "\"Not?A_Brand\";v=\"8\", \"Chromium\";v=\"108\"";
const SEC_CH_UA_PLATFORM: &str = "\"macOS\"";

fn client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .gzip(true)
            .brotli(true)
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("http client")
    })
}

pub fn perform_http(ctx: &AppContext, body: &Value) -> Value {
    let Some(url) = body.get("url").and_then(|v| v.as_str()) else {
        return json!({ "error": "bad url" });
    };
    let method = body.get("method").and_then(|v| v.as_str()).unwrap_or("GET").to_uppercase();
    let Ok(parsed_method) = reqwest::Method::from_bytes(method.as_bytes()) else {
        return json!({ "error": "bad method" });
    };

    // The renderer's own headers stay first; these fill the gaps a browser would
    // have set itself (the renderer ran from file:// in Electron, so no Origin or
    // Referer is emitted, matching the captured wire traffic).
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(map) = body.get("headers").and_then(|v| v.as_object()) {
        for (key, value) in map {
            if let Some(text) = value.as_str() {
                headers.push((key.clone(), text.to_string()));
            }
        }
    }
    for (name, value) in [
        ("user-agent", ctx.bridge.client_version_ua()),
        ("accept-language", ACCEPT_LANGUAGE.to_string()),
        ("sec-ch-ua", SEC_CH_UA.to_string()),
        ("sec-ch-ua-mobile", "?0".to_string()),
        ("sec-ch-ua-platform", SEC_CH_UA_PLATFORM.to_string()),
    ] {
        headers.push((name.to_string(), value));
    }

    // Cookies live in the shared jar so the renderer's login cookies and the
    // bridge's `setAppCookie` see the same session.
    let cookie_header = ctx.bridge.cookie_header(url);
    if !cookie_header.is_empty() {
        headers.push(("cookie".to_string(), cookie_header));
    }

    let timeout = body.get("timeout").and_then(|v| v.as_u64()).unwrap_or(0);
    // The message-sync segments (`transfersync.zaloapp.com/api/transfer-sync-v2/download`)
    // are tens of megabytes each and arrive over whatever link the PC has. The
    // worker budgets them like an API call (5 s), which on a normal Wi-Fi link
    // aborts mid-download and surfaces as a sync-wide `NO_NETWORK` (1106). Give
    // this one host a real download budget; the request itself is unchanged.
    let timeout = if url.contains("transfersync.") {
        timeout.max(120_000)
    } else {
        timeout
    };
    let (payload, content_type_override) = match body.get("body") {
        Some(encoded) => match apply_body(encoded) {
            Ok((bytes, content_type)) => (bytes, content_type),
            Err(err) => return json!({ "error": err }),
        },
        None => (None, None),
    };
    // multipart luôn ghi đè Content-Type mà trang đoán (như NetworkProxy.swift).
    if let Some(content_type) = content_type_override {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
        headers.push(("content-type".to_string(), content_type));
    }

    let started = std::time::Instant::now();

    // Chromium 108's ClientHello, the official client's — rustls cannot produce
    // that shape (no GREASE, fixed extension order).
    if let Some(helper) = crate::fingerprint::helper(ctx) {
        let request = crate::fingerprint::Request {
            method: method.clone(),
            url: url.to_string(),
            headers: headers.clone(),
            body: payload.clone(),
            timeout_ms: timeout,
            follow: true,
        };
        match helper.perform(&request) {
            Ok(response) => {
                if ctx.debug {
                    eprintln!(
                        "[net] {} {} → {} ({} B, {:?}) qua helper {}",
                        method,
                        url,
                        response.status,
                        response.body.len(),
                        started.elapsed(),
                        helper.profile
                    );
                }
                return response_json(ctx, url, response);
            }
            Err(crate::fingerprint::Error::Unavailable(reason)) => {
                if ctx.debug {
                    eprintln!("[net] fingerprint helper không dùng được ({reason}) — request này đi bằng rustls");
                }
            }
            Err(crate::fingerprint::Error::Failed(message)) => {
                let timed_out = message.contains("timeout");
                if ctx.debug {
                    eprintln!("[net] {method} {url} → lỗi: {message}");
                }
                return json!({ "error": message, "timedOut": timed_out });
            }
        }
    }

    let mut request = client().request(parsed_method, url);
    for (name, value) in &headers {
        request = request.header(name.as_str(), value.as_str());
    }
    if timeout > 0 {
        request = request.timeout(std::time::Duration::from_millis(timeout));
    }
    if let Some(bytes) = payload {
        request = request.body(bytes);
    }

    match request.send() {
        Ok(response) => {
            let status = response.status();
            let final_url = response.url().to_string();
            let mut headers = Map::new();
            for (key, value) in response.headers().iter() {
                let entry = headers
                    .entry(key.as_str().to_string())
                    .or_insert_with(|| Value::String(String::new()));
                let text = value.to_str().unwrap_or_default();
                if let Value::String(existing) = entry {
                    if existing.is_empty() {
                        *existing = text.to_string();
                    } else {
                        existing.push_str(", ");
                        existing.push_str(text);
                    }
                }
            }
            for cookie in response.headers().get_all("set-cookie").iter() {
                if let Ok(text) = cookie.to_str() {
                    ctx.bridge.store_cookie(url, text);
                }
            }
            let mut bytes = Vec::new();
            let mut response = response;
            if response.read_to_end(&mut bytes).is_err() {
                return json!({ "error": "body read failed" });
            }
            if ctx.debug {
                eprintln!(
                    "[net] {} {} → {} ({} B, {:?})",
                    method,
                    url,
                    status.as_u16(),
                    bytes.len(),
                    started.elapsed()
                );
            }
            json!({
                "status": status.as_u16(),
                "statusText": status.canonical_reason().unwrap_or(""),
                "url": final_url,
                "headers": headers,
                "body": base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        }
        Err(err) => {
            let timed_out = err.is_timeout();
            if ctx.debug {
                eprintln!("[net] {method} {url} → lỗi: {err}");
            }
            json!({ "error": err.to_string(), "timedOut": timed_out })
        }
    }
}

/// Chromium 108 response JSON: one header map (duplicates joined), cookies into the
/// shared jar, body base64 — the shape `network-shim.js` expects from `znet`.
fn response_json(ctx: &AppContext, url: &str, response: crate::fingerprint::Response) -> Value {
    let mut headers = Map::new();
    for (key, value) in &response.headers {
        let name = key.to_ascii_lowercase();
        let entry = headers.entry(name).or_insert_with(|| Value::String(String::new()));
        if let Value::String(existing) = entry {
            if existing.is_empty() {
                *existing = value.clone();
            } else {
                existing.push_str(", ");
                existing.push_str(value);
            }
        }
    }
    for (key, value) in &response.headers {
        if key.eq_ignore_ascii_case("set-cookie") {
            ctx.bridge.store_cookie(url, value);
        }
    }
    if dump_login(url) {
        let end = response.body.len().min(3000);
        eprintln!(
            "[net] body {} {} → {}",
            response.status,
            url,
            String::from_utf8_lossy(&response.body[..end]).replace('\n', " ")
        );
    }
    json!({
        "status": response.status,
        "statusText": response.status_text,
        "url": response.url,
        "headers": headers,
        "body": base64::engine::general_purpose::STANDARD.encode(&response.body),
    })
}

/// `OLAZ_DUMP=1` prints what the login endpoints answered, which is the only way
/// to see why the server rejected a QR without a phone at hand.
fn dump_login(url: &str) -> bool {
    std::env::var("OLAZ_DUMP").map(|v| v == "1").unwrap_or(false)
        && (url.contains("/api/login/") || url.contains("qr-id."))
}

fn apply_body(encoded: &Value) -> Result<(Option<Vec<u8>>, Option<String>), String> {
    match encoded.get("kind").and_then(|v| v.as_str()) {
        Some("text") => {
            let text = encoded.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
            Ok((Some(text.into_bytes()), None))
        }
        Some("base64") => {
            let data = encoded.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map(Some)
                .map_err(|e| format!("bad body base64: {e}"))?;
            Ok((bytes, None))
        }
        // Dựng lại multipart body đúng như `NetworkProxy.swift` trên macOS:
        // boundary kiểu Chromium, part text/file, và Content-Type luôn ghi đè
        // thứ trang đã đoán.
        Some("formdata") => {
            let boundary = chromium_form_boundary();
            let mut body: Vec<u8> = Vec::new();
            for part in encoded.get("parts").and_then(Value::as_array).unwrap_or(&Vec::new()) {
                let name = part.get("name").and_then(Value::as_str).unwrap_or("");
                body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
                match part.get("filename").and_then(Value::as_str) {
                    Some(filename) => {
                        body.extend_from_slice(
                            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n").as_bytes(),
                        );
                        let content_type = part
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("application/octet-stream");
                        body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
                        let data = part.get("data").and_then(Value::as_str).unwrap_or("");
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .map_err(|e| format!("bad part base64 ({name}): {e}"))?;
                        body.extend_from_slice(&bytes);
                    }
                    None => {
                        body.extend_from_slice(
                            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                        );
                        body.extend_from_slice(part.get("value").and_then(Value::as_str).unwrap_or("").as_bytes());
                    }
                }
                body.extend_from_slice(b"\r\n");
            }
            body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
            Ok((Some(body), Some(format!("multipart/form-data; boundary={boundary}"))))
        }
        _ => Ok((None, None)),
    }
}

/// Boundary kiểu Chromium 108 (`----WebKitFormBoundary` + 16 ký tự), giống
/// `NetworkProxy.chromiumFormBoundary()` của bản macOS.
fn chromium_form_boundary() -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut boundary = String::from("----WebKitFormBoundary");
    for _ in 0..16 {
        let index = rand::Rng::gen_range(&mut rand::thread_rng(), 0..ALPHABET.len());
        boundary.push(ALPHABET[index] as char);
    }
    boundary
}

// ------------------------------------------------------------------ WebSockets

enum WsCommand {
    Send(tungstenite::Message),
    Close,
}

/// One live socket. The generation matters: the page numbers sockets from 1 on
/// every load, so after a navigation a new socket can reuse the id of one whose
/// reader thread is still winding down. Removing by id alone let that stale
/// thread drop the *new* sender, after which every `ws.send` vanished silently
/// and the realtime handshake never reached the server.
struct SocketEntry {
    generation: u64,
    sender: mpsc::Sender<WsCommand>,
}

fn sockets() -> &'static Mutex<HashMap<i64, SocketEntry>> {
    static SOCKETS: OnceLock<Mutex<HashMap<i64, SocketEntry>>> = OnceLock::new();
    SOCKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_socket(id: i64) -> (mpsc::Receiver<WsCommand>, u64) {
    static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let generation = NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (sender, receiver) = mpsc::channel();
    sockets().lock().unwrap().insert(id, SocketEntry { generation, sender });
    (receiver, generation)
}

/// Only the reader thread that registered this generation may remove the id.
fn finish_socket(id: i64, generation: u64) {
    let mut map = sockets().lock().unwrap();
    if map.get(&id).map(|entry| entry.generation) == Some(generation) {
        map.remove(&id);
    }
}

pub fn ws_open(ctx: &AppContext, body: &Value) -> Result<(), String> {
    let id = body.get("id").and_then(|v| v.as_i64()).ok_or("ws.open: thiếu id")?;
    let url = body.get("url").and_then(|v| v.as_str()).ok_or("ws.open: thiếu url")?.to_string();
    if ctx.debug {
        eprintln!("[net] ws#{id} mở {url}");
    }
    let protocols: Vec<String> = body
        .get("protocols")
        .and_then(|v| v.as_array())
        .map(|list| list.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    // Electron serialised the file:// page origin this way; Zalo's realtime
    // endpoint checks it (NetworkProxy.websocketOrigin on macOS).
    let mut headers: Vec<(String, String)> = vec![
        ("Origin".to_string(), "file://".to_string()),
        ("User-Agent".to_string(), ctx.bridge.client_version_ua()),
    ];
    // On macOS URLSession attached the shared cookie storage to the upgrade;
    // without the session cookie the realtime endpoint upgrades anonymously and
    // then never delivers anything, which looks exactly like "no sync".
    let cookie = ctx.bridge.cookie_header(&url);
    if ctx.debug {
        eprintln!("[net] ws#{id} cookie {} B", cookie.len());
    }
    if !cookie.is_empty() {
        headers.push(("Cookie".to_string(), cookie));
    }
    if !protocols.is_empty() {
        headers.push(("Sec-WebSocket-Protocol".to_string(), protocols.join(", ")));
    }

    let (rx, generation) = register_socket(id);
    let events = ctx.events.clone();
    let debug = ctx.debug;

    // The upgrade carries the same Chrome 108 ClientHello as HTTP, through the
    // helper. Verified against a local socket: WSOPENED comes back and frames
    // flow, so this is the normal path; tungstenite is the fallback for when the
    // helper cannot start at all.
    if let Some(helper) = crate::fingerprint::helper(ctx) {
        std::thread::spawn(move || {
            if !open_via_helper(&helper, &format!("w{id}"), id, &url, &headers, &protocols, &events, debug, &rx) {
                open_via_tungstenite(id, &url, &headers, &protocols, &events, debug, rx);
            }
            finish_socket(id, generation);
        });
        return Ok(());
    }

    std::thread::spawn(move || {
        open_via_tungstenite(id, &url, &headers, &protocols, &events, debug, rx);
        finish_socket(id, generation);
    });
    Ok(())
}

/// Tungstenite + rustls: the fallback transport for the realtime socket.
#[allow(clippy::too_many_arguments)]
fn open_via_tungstenite(
    id: i64,
    url: &str,
    headers: &[(String, String)],
    protocols: &[String],
    events: &Events,
    debug: bool,
    commands: mpsc::Receiver<WsCommand>,
) {
    use tungstenite::client::IntoClientRequest;

    let mut request = match url.into_client_request() {
        Ok(request) => request,
        Err(error) => {
            events.push(json!({ "kind": "socket", "id": id, "event": "close", "payload": { "code": 1006, "reason": error.to_string(), "wasClean": false } }));
            return;
        }
    };
    for (name, value) in headers {
        let (Ok(name), Ok(value)) = (
            name.parse::<tungstenite::http::HeaderName>(),
            value.parse::<tungstenite::http::HeaderValue>(),
        ) else {
            continue;
        };
        request.headers_mut().insert(name, value);
    }
    if !protocols.is_empty() {
        if let Ok(value) = protocols.join(", ").parse() {
            request.headers_mut().insert("Sec-WebSocket-Protocol", value);
        }
    }

    {
        let connected = tungstenite::connect(request);
        let (mut socket, response) = match connected {
            Ok(pair) => pair,
            Err(err) => {
                if debug {
                    eprintln!("[net] ws#{id} handshake lỗi: {err} ({url})");
                }
                events.push(json!({ "kind": "socket", "id": id, "event": "close", "payload": { "code": 1006, "reason": err.to_string(), "wasClean": false } }));
                return;
            }
        };
        // The command loop shares this thread with `read()`. A blocking read would
        // hold an outgoing frame (the realtime handshake or an ack) until the next
        // server frame arrived, so keep reads short and treat a timeout as "idle".
        let timed = set_read_timeout(&socket, 100);
        if debug && !timed {
            eprintln!("[net] ws#{id} không đặt được SO_RCVTIMEO — lệnh gửi có thể bị chậm");
        }
        let protocol = response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if debug {
            eprintln!("[net] ws#{id} handshake ok ({protocol})");
        }
        events.push(json!({ "kind": "socket", "id": id, "event": "open", "payload": { "protocol": protocol } }));

        let mut frames = 0usize;
        loop {
            match commands.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(WsCommand::Send(message)) => {
                    trace_message(debug, id, "out", &message, &mut frames);
                    if let Err(err) = socket.send(message) {
                        if debug {
                            eprintln!("[net] ws#{id} gửi lỗi: {err}");
                        }
                        events.push(json!({ "kind": "socket", "id": id, "event": "close", "payload": { "code": 1006, "reason": err.to_string(), "wasClean": false } }));
                        break;
                    }
                }
                Ok(WsCommand::Close) => {
                    let _ = socket.close(None);
                    events.push(json!({ "kind": "socket", "id": id, "event": "close", "payload": { "code": 1000, "reason": "", "wasClean": true } }));
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            match socket.read() {
                Ok(tungstenite::Message::Binary(bytes)) => {
                    trace_frame(debug, id, "in", &bytes, &mut frames);
                    events.push(json!({ "kind": "socket", "id": id, "event": "binary", "payload": { "data": base64::engine::general_purpose::STANDARD.encode(&bytes) } }));
                }
                Ok(tungstenite::Message::Text(text)) => {
                    trace_text(debug, id, "in", text.len(), &mut frames);
                    events.push(json!({ "kind": "socket", "id": id, "event": "text", "payload": { "text": text } }));
                }
                Ok(tungstenite::Message::Ping(payload)) => {
                    let _ = socket.send(tungstenite::Message::Pong(payload));
                }
                Ok(tungstenite::Message::Close(frame)) => {
                    let (code, reason) = frame
                        .map(|f| (u16::from(f.code), f.reason.to_string()))
                        .unwrap_or((1000, String::new()));
                    if debug {
                        eprintln!("[net] ws#{id} server đóng {code} {reason}");
                    }
                    events.push(json!({ "kind": "socket", "id": id, "event": "close", "payload": { "code": code, "reason": reason, "wasClean": true } }));
                    break;
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(ref e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(err) => {
                    if debug {
                        eprintln!("[net] ws#{id} đọc lỗi: {err}");
                    }
                    events.push(json!({ "kind": "socket", "id": id, "event": "close", "payload": { "code": 1006, "reason": err.to_string(), "wasClean": false } }));
                    break;
                }
            }
        }
    }
}

/// One realtime socket through the fingerprint helper. Returns false when the
/// helper could not even complete the upgrade, so the caller can dial it with
/// tungstenite; once open, failures are real results and are reported as closes.
#[allow(clippy::too_many_arguments)]
fn open_via_helper(
    helper: &crate::fingerprint::Helper,
    key: &str,
    id: i64,
    url: &str,
    headers: &[(String, String)],
    protocols: &[String],
    events: &Events,
    debug: bool,
    commands: &mpsc::Receiver<WsCommand>,
) -> bool {
    use crate::fingerprint::WsEvent;

    let close = |code: u32, reason: String| {
        json!({ "kind": "socket", "id": id, "event": "close",
                "payload": { "code": code, "reason": reason, "wasClean": code != 1006 } })
    };

    let (sink, incoming) = mpsc::channel::<WsEvent>();
    if let Err(error) = helper.ws_open(key, url, headers, protocols, sink) {
        let reason = match error {
            crate::fingerprint::Error::Unavailable(message)
            | crate::fingerprint::Error::Failed(message) => message,
        };
        if debug {
            eprintln!("[net] ws#{id} helper không mở được ({reason}) — thử tungstenite");
        }
        return false;
    }

    // The first event is the handshake result.
    let protocol = match incoming.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(WsEvent::Opened(protocol)) => protocol,
        Ok(WsEvent::Failed(reason)) => {
            if debug {
                eprintln!("[net] ws#{id} helper từ chối upgrade ({reason}) — thử tungstenite");
            }
            return false;
        }
        _ => return false,
    };
    if debug {
        eprintln!("[net] ws#{id} handshake ok qua helper ({protocol})");
    }
    events.push(json!({ "kind": "socket", "id": id, "event": "open", "payload": { "protocol": protocol } }));

    let mut frames = 0usize;
    loop {
        match commands.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(WsCommand::Send(message)) => {
                trace_message(debug, id, "out", &message, &mut frames);
                match message {
                    tungstenite::Message::Text(text) => helper.ws_send(&key, Some(&text), None),
                    tungstenite::Message::Binary(bytes) => helper.ws_send(&key, None, Some(&bytes)),
                    _ => {}
                }
            }
            Ok(WsCommand::Close) => {
                helper.ws_close(&key, 1000);
                events.push(close(1000, String::new()));
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        match incoming.try_recv() {
            Ok(WsEvent::Text(text)) => {
                trace_text(debug, id, "in", text.len(), &mut frames);
                events.push(json!({ "kind": "socket", "id": id, "event": "text", "payload": { "text": text } }));
            }
            Ok(WsEvent::Binary(bytes)) => {
                trace_frame(debug, id, "in", &bytes, &mut frames);
                events.push(json!({ "kind": "socket", "id": id, "event": "binary", "payload": { "data": base64::engine::general_purpose::STANDARD.encode(&bytes) } }));
            }
            Ok(WsEvent::Closed(code, reason)) => {
                if debug {
                    eprintln!("[net] ws#{id} helper đóng {code} {reason}");
                }
                events.push(close(code, reason));
                break;
            }
            Ok(WsEvent::Failed(reason)) => {
                if debug {
                    eprintln!("[net] ws#{id} helper lỗi: {reason}");
                }
                events.push(close(1006, reason));
                break;
            }
            Ok(WsEvent::Opened(_)) => {}
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }
    true
}

pub fn ws_send(body: &Value) {
    use base64::Engine as _;
    let Some(id) = body.get("id").and_then(|v| v.as_i64()) else { return };
    let message = match (body.get("text").and_then(|v| v.as_str()), body.get("data").and_then(|v| v.as_str())) {
        (Some(text), _) => tungstenite::Message::Text(text.to_string()),
        (None, Some(data)) => match base64::engine::general_purpose::STANDARD.decode(data) {
            Ok(bytes) => tungstenite::Message::Binary(bytes),
            Err(_) => return,
        },
        _ => return,
    };
    if let Some(entry) = sockets().lock().unwrap().get(&id) {
        let _ = entry.sender.send(WsCommand::Send(message));
    }
}

pub fn ws_close(body: &Value) {
    let Some(id) = body.get("id").and_then(|v| v.as_i64()) else { return };
    if let Some(entry) = sockets().lock().unwrap().get(&id) {
        let _ = entry.sender.send(WsCommand::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formdata_body_is_built_like_chromium() {
        use base64::Engine;
        let encoded = json!({
            "kind": "formdata",
            "parts": [
                { "name": "file", "filename": "report.pdf", "type": "application/pdf",
                  "data": base64::engine::general_purpose::STANDARD.encode(b"%PDF-hello") },
                { "name": "note", "value": "xin chào" }
            ]
        });
        let (body, content_type) = apply_body(&encoded).unwrap();
        let content_type = content_type.unwrap();
        assert!(content_type.starts_with("multipart/form-data; boundary=----WebKitFormBoundary"));
        let boundary = content_type.rsplit('=').next().unwrap();
        assert_eq!(boundary.len(), "----WebKitFormBoundary".len() + 16);
        let body = body.unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("Content-Disposition: form-data; name=\"file\"; filename=\"report.pdf\"\r\n"));
        assert!(text.contains("Content-Type: application/pdf\r\n"));
        assert!(text.contains("%PDF-hello"));
        assert!(text.contains("Content-Disposition: form-data; name=\"note\"\r\n\r\nxin chào"));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
        // mọi separator đều dùng đúng boundary
        assert_eq!(text.matches(boundary).count(), 3);
    }

    #[test]
    fn text_body_has_no_content_type_override() {
        let (body, content_type) = apply_body(&json!({ "kind": "text", "text": "hello" })).unwrap();
        assert_eq!(body.unwrap(), b"hello");
        assert!(content_type.is_none());
    }

    #[test]
    fn multi_file_and_mixed_parts_are_all_encoded() {
        use base64::Engine;
        let engine = base64::engine::general_purpose::STANDARD;
        // Một chunk-upload thật: header text + nhiều file part trong cùng body.
        let encoded = json!({
            "kind": "formdata",
            "parts": [
                { "name": "chunkId", "value": "7" },
                { "name": "chunkContent", "filename": "part.bin", "type": "application/octet-stream",
                  "data": engine.encode(b"chunk-one") },
                { "name": "chunkContent", "filename": "part.bin", "type": "application/octet-stream",
                  "data": engine.encode(b"chunk-two") },
                { "name": "avatar", "filename": "a.png", "type": "image/png",
                  "data": engine.encode([0u8, 159, 146, 150]) }
            ]
        });
        let (body, content_type) = apply_body(&encoded).unwrap();
        let content_type = content_type.unwrap();
        let boundary = content_type.rsplit('=').next().unwrap();
        let body = body.unwrap();
        let text = String::from_utf8_lossy(&body);
        // 3 file part + 1 text part, mỗi cái một khối Content-Disposition
        assert_eq!(text.matches("Content-Disposition: form-data; name=\"chunkContent\"").count(), 2);
        assert!(text.contains("chunk-one"));
        assert!(text.contains("chunk-two"));
        assert!(text.contains("filename=\"a.png\""));
        assert!(text.contains("Content-Type: image/png"));
        // byte nhị phân (không phải UTF-8) phải giữ nguyên qua base64
        assert!(body.windows(4).any(|w| w == [0, 159, 146, 150]));
        assert_eq!(text.matches(boundary).count(), 5);
    }

    #[test]
    fn frame_header_is_described_without_the_body() {
        // [version=1][cmd=0x0102 LE][sub=7] + a body that must never be printed.
        let frame = [1u8, 0x02, 0x01, 7, 0xde, 0xad, 0xbe, 0xef];
        let described = describe_frame(&frame);
        assert_eq!(described, "v=1 cmd=258 sub=7 len=8");
        assert!(!described.contains("de") && !described.contains("ad"));
        assert_eq!(describe_frame(&[1, 2]), "len=2");
    }

    #[test]
    fn stale_socket_generation_keeps_the_new_socket() {
        let (_old, old_generation) = register_socket(9001);
        let (_new, new_generation) = register_socket(9001);
        assert_ne!(old_generation, new_generation);
        finish_socket(9001, old_generation);
        assert!(sockets().lock().unwrap().contains_key(&9001));
        finish_socket(9001, new_generation);
        assert!(!sockets().lock().unwrap().contains_key(&9001));
    }
}

// ------------------------------------------------------------------ frame trace

/// `OLAZ_DEBUG=1` prints the realtime frame *header* only — never the body:
/// Zalo frames start with `[version u8][cmd u16 LE][subCmd u8]`, the same fields
/// `NetworkProxy.describeFrame` traces on macOS.
fn describe_frame(data: &[u8]) -> String {
    if data.len() < 4 {
        return format!("len={}", data.len());
    }
    let cmd = u16::from_le_bytes([data[1], data[2]]);
    format!("v={} cmd={} sub={} len={}", data[0], cmd, data[3], data.len())
}

/// Same 80-frame budget per socket as the macOS host, so a long-lived realtime
/// connection cannot flood the terminal.
fn trace_frame(debug: bool, id: i64, direction: &str, data: &[u8], frames: &mut usize) {
    if !debug {
        return;
    }
    *frames += 1;
    if *frames <= 80 {
        eprintln!("[net] ws#{id} {direction} {}", describe_frame(data));
    }
}

/// Text frames carry no Zalo header (the realtime channel is binary), so only the
/// size is logged — again, never the content.
fn trace_text(debug: bool, id: i64, direction: &str, len: usize, frames: &mut usize) {
    if !debug {
        return;
    }
    *frames += 1;
    if *frames <= 80 {
        eprintln!("[net] ws#{id} {direction} text len={len}");
    }
}

fn trace_message(debug: bool, id: i64, direction: &str, message: &tungstenite::Message, frames: &mut usize) {
    match message {
        tungstenite::Message::Binary(bytes) => trace_frame(debug, id, direction, bytes, frames),
        tungstenite::Message::Text(text) => trace_text(debug, id, direction, text.len(), frames),
        _ => {}
    }
}

/// Short reads through `SO_RCVTIMEO`. Works for both plain TCP and the rustls
/// stream tungstenite hands back, without depending on tungstenite's config API.
fn set_read_timeout(
    socket: &tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    millis: u64,
) -> bool {
    use std::os::fd::AsRawFd;
    use tungstenite::stream::MaybeTlsStream;
    let fd = match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => stream.as_raw_fd(),
        MaybeTlsStream::Rustls(stream) => stream.sock.as_raw_fd(),
        _ => return false,
    };
    let timeout = libc::timeval {
        tv_sec: (millis / 1000) as libc::time_t,
        tv_usec: ((millis % 1000) * 1000) as libc::suseconds_t,
    };
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &timeout as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        ) == 0
    }
}
