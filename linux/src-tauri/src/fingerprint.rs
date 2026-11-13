//! Chromium 108's TLS + HTTP/2 fingerprint, from the repository's Go helper.
//!
//! The official Zalo PC client is Electron 22.3.9 (Chromium 108), and VNG's servers
//! fingerprint the connection itself — JA3/JA4, extension order, HTTP/2 SETTINGS.
//! rustls cannot reproduce that shape (no GREASE, fixed extension order), so the
//! macOS build spawns `FingerprintHelper/` (Go, tls-client `Chrome_108` profile)
//! and sends every request through it. This is the Linux counterpart: the very
//! same helper source built for linux/amd64, bundled next to the executable, and
//! spoken to over a unix socket with the protocol documented in
//! `FingerprintHelper/protocol.go`. `FingerprintProxy.swift` is the Swift client;
//! this file is the Rust one.
//!
//! When the helper cannot start (missing binary, sandbox, crash) `helper()` returns
//! `None` and `net.rs` falls back to rustls for that request — the same fallback
//! macOS makes to URLSession. An upstream failure is *not* retried on the other
//! stack: that would switch fingerprints mid-session, which is what this exists to
//! avoid.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

use crate::host::AppContext;

/// Profile the helper reports on `HELLO` (tls-client's `profiles.Chrome_108`).
pub const PROFILE: &str = "chrome_108";

pub enum Error {
    /// The helper is not running (and could not be started). Fall back to rustls.
    Unavailable(String),
    /// The upstream request itself failed — a real result, do not fall back.
    Failed(String),
}

pub struct Request {
    pub method: String,
    pub url: String,
    /// Sent in this order (the renderer's own headers first, then Chromium's defaults).
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout_ms: u64,
    pub follow: bool,
}

pub struct Response {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub url: String,
    pub body: Vec<u8>,
}

/// One websocket event, as the helper reports it (`WSOPENED`/`WSMSG`/`WSCLOSED`/`WSERR`).
pub enum WsEvent {
    Opened(String),
    Text(String),
    Binary(Vec<u8>),
    Closed(u32, String),
    Failed(String),
}

enum Reply {
    Response(Value),
    Failure(Value),
}

struct Shared {
    pending: Mutex<HashMap<u64, mpsc::Sender<Reply>>>,
    sockets: Mutex<HashMap<String, mpsc::Sender<WsEvent>>>,
    hello: Mutex<Option<mpsc::Sender<String>>>,
}

pub struct Helper {
    writer: Mutex<UnixStream>,
    shared: Arc<Shared>,
    next: AtomicU64,
    child: Mutex<Option<Child>>,
    socket: PathBuf,
    pub profile: String,
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.send_line("SHUTDOWN\n");
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

// ------------------------------------------------------------------ lifecycle

struct State {
    helper: Option<Arc<Helper>>,
    retry_at: Instant,
    attempts: u32,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State { helper: None, retry_at: Instant::now(), attempts: 0 })
    })
}

/// The shared helper, started on first use. `None` means "use rustls for this one".
pub fn helper(ctx: &AppContext) -> Option<Arc<Helper>> {
    if std::env::var("OLAZ_FINGERPRINT").map(|value| value == "0").unwrap_or(false) {
        return None;
    }
    let mut state = state().lock().unwrap();
    if let Some(helper) = &state.helper {
        return Some(helper.clone());
    }
    if Instant::now() < state.retry_at {
        return None;
    }
    state.attempts += 1;
    let attempts = state.attempts;
    match start(ctx) {
        Ok(helper) => {
            let helper = Arc::new(helper);
            if ctx.debug {
                eprintln!("[fp] fingerprint helper sẵn sàng ({})", helper.profile);
            }
            state.helper = Some(helper.clone());
            state.attempts = 0;
            Some(helper)
        }
        Err(error) => {
            let delay = Duration::from_secs(2u64.pow(attempts.min(5)));
            state.retry_at = Instant::now() + delay;
            eprintln!("[fp] không dùng được fingerprint helper: {error} — request sẽ đi bằng rustls");
            None
        }
    }
}

/// Returns the app-owned helper only; callers must not start or take ownership of it.
pub fn running_helper() -> Option<Arc<Helper>> {
    state().lock().unwrap().helper.clone()
}

/// Starts the helper early so the first request does not wait for it.
pub fn warm_up(ctx: Arc<AppContext>) {
    if std::env::var("OLAZ_FINGERPRINT").map(|value| value == "0").unwrap_or(false) {
        return;
    }
    std::thread::spawn(move || {
        let _ = helper(&ctx);
    });
}

fn start(ctx: &AppContext) -> Result<Helper, String> {
    let binary = helper_binary().ok_or_else(|| {
        "không thấy olaz-fingerprint-helper (chạy linux/scripts/build-fingerprint-helper.sh, \
         hoặc đặt OLAZ_FINGERPRINT_HELPER=/đường/dẫn)"
            .to_string()
    })?;
    let _ = std::fs::create_dir_all(&ctx.userdata);
    let socket = socket_path(ctx);
    let _ = std::fs::remove_file(&socket);

    let mut child = Command::new(&binary)
        .arg("-socket")
        .arg(&socket)
        // The helper outlives requests; it must not inherit our stdout (a test
        // harness reading that pipe would wait for it forever).
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("spawn {}: {error}", binary.display()))?;

    // Up to 5 s for the socket to appear, while watching for an early exit.
    let mut appeared = false;
    for _ in 0..50 {
        if socket.exists() {
            appeared = true;
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("helper thoát ngay ({status})"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !appeared {
        let _ = child.kill();
        return Err(format!("helper không mở socket {}", socket.display()));
    }

    let stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = child.kill();
            return Err(format!("connect {}: {error}", socket.display()));
        }
    };
    let reader = stream.try_clone().map_err(|error| format!("clone socket: {error}"))?;

    let shared = Arc::new(Shared {
        pending: Mutex::new(HashMap::new()),
        sockets: Mutex::new(HashMap::new()),
        hello: Mutex::new(None),
    });
    let (hello_tx, hello_rx) = mpsc::channel();
    *shared.hello.lock().unwrap() = Some(hello_tx);
    let debug = ctx.debug;
    let reader_shared = shared.clone();
    std::thread::Builder::new()
        .name("olaz-fp-reader".into())
        .spawn(move || read_loop(reader, reader_shared, debug))
        .map_err(|error| format!("reader thread: {error}"))?;

    let helper = Helper {
        writer: Mutex::new(stream),
        shared,
        next: AtomicU64::new(0),
        child: Mutex::new(Some(child)),
        socket,
        profile: PROFILE.to_string(),
    };
    helper
        .send_line("HELLO\n")
        .map_err(|error| format!("HELLO: {error}"))?;
    match hello_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(reply) => {
            // "OK 1 chrome_108"
            if let Some(profile) = reply.split_whitespace().last() {
                let mut helper = helper;
                helper.profile = profile.to_string();
                return Ok(helper);
            }
            Err(format!("HELLO lạ: {reply}"))
        }
        Err(_) => Err("helper không trả lời HELLO".to_string()),
    }
}

fn socket_path(ctx: &AppContext) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "fp-helper-{}-{}.sock",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let path = ctx.userdata.join(&name);
    // sun_path is 108 bytes on Linux; a deep userData path would not fit.
    if path.as_os_str().len() > 100 {
        std::env::temp_dir().join(name)
    } else {
        path
    }
}

fn helper_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("OLAZ_FINGERPRINT_HELPER") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    // Bundled: Tauri places `externalBin` next to the app's executable.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(directory) = exe.parent() {
            for name in ["olaz-fingerprint-helper", "olaz-fingerprint-helper-x86_64-unknown-linux-gnu"] {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    // `cargo run`/tests: the binary the build script leaves in the repository.
    let dev = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("binaries/olaz-fingerprint-helper-x86_64-unknown-linux-gnu");
    if dev.is_file() {
        return Some(dev);
    }
    None
}

// ---------------------------------------------------------------- wire reading

fn read_loop(stream: UnixStream, shared: Arc<Shared>, debug: bool) {
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = line.trim_end_matches(['\n', '\r']);
        let mut parts = line.splitn(3, ' ');
        let command = parts.next().unwrap_or("");
        let id = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        match command {
            "OK" => {
                if let Some(sink) = shared.hello.lock().unwrap().take() {
                    let _ = sink.send(rest.to_string());
                }
            }
            "RES" | "ERR" => {
                let Ok(value) = serde_json::from_str::<Value>(rest) else { continue };
                let reply = if command == "RES" { Reply::Response(value) } else { Reply::Failure(value) };
                let id = id.parse::<u64>().unwrap_or(0);
                if let Some(sink) = shared.pending.lock().unwrap().remove(&id) {
                    let _ = sink.send(reply);
                }
            }
            "WSOPENED" | "WSERR" => {
                let Ok(value) = serde_json::from_str::<Value>(rest) else { continue };
                let event = if command == "WSOPENED" {
                    WsEvent::Opened(text(&value, "protocol"))
                } else {
                    WsEvent::Failed(text(&value, "error"))
                };
                if let Some(sink) = shared.sockets.lock().unwrap().get(id) {
                    let _ = sink.send(event);
                }
            }
            "WSMSG" => {
                // rest = "text|binary <len>", payload follows the newline.
                let mut fields = rest.splitn(2, ' ');
                let kind = fields.next().unwrap_or("binary");
                let Ok(length) = fields.next().unwrap_or("0").parse::<usize>() else { continue };
                let mut payload = vec![0u8; length];
                if reader.read_exact(&mut payload).is_err() {
                    break;
                }
                let event = if kind == "text" {
                    WsEvent::Text(String::from_utf8_lossy(&payload).into_owned())
                } else {
                    WsEvent::Binary(payload)
                };
                if let Some(sink) = shared.sockets.lock().unwrap().get(id) {
                    let _ = sink.send(event);
                }
            }
            "WSCLOSED" => {
                // rest = "<code> {json}"
                let mut fields = rest.splitn(2, ' ');
                let code = fields.next().unwrap_or("1006").parse::<u32>().unwrap_or(1006);
                let reason = fields
                    .next()
                    .and_then(|json| serde_json::from_str::<Value>(json).ok())
                    .map(|value| text(&value, "reason"))
                    .unwrap_or_default();
                if let Some(sink) = shared.sockets.lock().unwrap().remove(id) {
                    let _ = sink.send(WsEvent::Closed(code, reason));
                }
            }
            "BYE" => break,
            _ => {}
        }
    }
    if debug {
        eprintln!("[fp] helper đã đóng kết nối");
    }
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

// -------------------------------------------------------------------- requests

impl Helper {
    fn send_frame(&self, line: &str, payload: Option<&[u8]>) -> std::io::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(line.as_bytes())?;
        if let Some(payload) = payload {
            writer.write_all(payload)?;
        }
        writer.flush()
    }

    fn send_line(&self, line: &str) -> std::io::Result<()> {
        self.send_frame(line, None)
    }

    pub fn perform(&self, request: &Request) -> Result<Response, Error> {
        self.perform_cancelable(request, None)
    }

    pub fn perform_cancelable(
        &self,
        request: &Request,
        cancel: Option<&AtomicBool>,
    ) -> Result<Response, Error> {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel();
        self.shared.pending.lock().unwrap().insert(id, tx);

        let headers: Vec<Value> = request
            .headers
            .iter()
            .map(|(name, value)| json!([name, value]))
            .collect();
        let body = request
            .body
            .as_ref()
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes))
            .unwrap_or_default();
        let spec = json!({
            "method": request.method,
            "url": request.url,
            "headers": headers,
            "bodyB64": body,
            "timeoutMs": request.timeout_ms,
            "stream": false,
            "follow": request.follow,
            "maxBytes": 0,
            "idleMs": 30000,
            "firstByteMs": 0,
        });

        if let Err(error) = self.send_line(&format!("REQ {id} {spec}\n")) {
            self.shared.pending.lock().unwrap().remove(&id);
            return Err(Error::Unavailable(format!("helper write: {error}")));
        }

        let wait = if request.timeout_ms > 0 {
            Duration::from_millis(request.timeout_ms + 10_000)
        } else {
            Duration::from_secs(600)
        };
        let deadline = Instant::now() + wait;
        let reply = loop {
            if cancel
                .map(|flag| flag.load(Ordering::Relaxed))
                .unwrap_or(false)
            {
                let _ = self.send_line(&format!("CANCEL {id}\n"));
                self.shared.pending.lock().unwrap().remove(&id);
                return Err(Error::Failed("canceled".to_string()));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.send_line(&format!("CANCEL {id}\n"));
                self.shared.pending.lock().unwrap().remove(&id);
                return Err(Error::Failed(if request.timeout_ms > 0 {
                    "timeout".to_string()
                } else {
                    "fingerprint helper không trả lời".to_string()
                }));
            }
            let poll = if cancel.is_some() {
                remaining.min(Duration::from_millis(100))
            } else {
                remaining
            };
            match rx.recv_timeout(poll) {
                Ok(reply) => break reply,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.shared.pending.lock().unwrap().remove(&id);
                    return Err(Error::Unavailable("fingerprint helper đã đóng kết nối".into()));
                }
            }
        };

        match reply {
            Reply::Failure(value) => Err(Error::Failed(text(&value, "error"))),
            Reply::Response(value) => Ok(response_from(value)),
        }
    }

    /// Registers `sink` and asks the helper to dial. The first `WsEvent` on `sink` is
    /// `Opened` or `Failed` (the handshake result).
    pub fn ws_open(
        &self,
        key: &str,
        url: &str,
        headers: &[(String, String)],
        protocols: &[String],
        sink: mpsc::Sender<WsEvent>,
    ) -> Result<(), Error> {
        self.shared.sockets.lock().unwrap().insert(key.to_string(), sink);
        let headers: Vec<Value> = headers.iter().map(|(name, value)| json!([name, value])).collect();
        let spec = json!({ "url": url, "headers": headers, "protocols": protocols });
        if let Err(error) = self.send_line(&format!("WSOPEN {key} {spec}\n")) {
            self.shared.sockets.lock().unwrap().remove(key);
            return Err(Error::Unavailable(format!("helper write: {error}")));
        }
        Ok(())
    }

    pub fn ws_send(&self, key: &str, text: Option<&str>, binary: Option<&[u8]>) {
        match (text, binary) {
            (Some(text), _) => {
                let _ = self.send_frame(&format!("WSSEND {key} text {}\n", text.len()), Some(text.as_bytes()));
            }
            (None, Some(binary)) => {
                let _ = self.send_frame(&format!("WSSEND {key} binary {}\n", binary.len()), Some(binary));
            }
            (None, None) => {}
        }
    }

    pub fn ws_close(&self, key: &str, code: u32) {
        let _ = self.send_line(&format!("WSCLOSE {key} {code}\n"));
        self.shared.sockets.lock().unwrap().remove(key);
    }
}

fn response_from(value: Value) -> Response {
    let status = value.get("status").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    let raw = text(&value, "statusText");
    let mut headers = Vec::new();
    if let Some(list) = value.get("headers").and_then(|v| v.as_array()) {
        for pair in list {
            let Some(pair) = pair.as_array() else { continue };
            if pair.len() != 2 {
                continue;
            }
            if let (Some(name), Some(text)) = (pair[0].as_str(), pair[1].as_str()) {
                headers.push((name.to_string(), text.to_string()));
            }
        }
    }
    let body = value
        .get("bodyB64")
        .and_then(|v| v.as_str())
        .map(|encoded| base64::engine::general_purpose::STANDARD.decode(encoded).unwrap_or_default())
        .unwrap_or_default();
    Response {
        status,
        status_text: status_text(status, &raw),
        headers,
        url: text(&value, "url"),
        body,
    }
}

/// The helper reports Go's `"200 OK"`; the renderer expects the reason phrase alone.
fn status_text(status: u16, raw: &str) -> String {
    match raw.split_once(' ') {
        Some((prefix, rest)) if prefix.parse::<u16>() == Ok(status) => rest.to_string(),
        _ => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::status_text;

    #[test]
    fn status_text_strips_the_code() {
        assert_eq!(status_text(200, "200 OK"), "OK");
        assert_eq!(status_text(404, "404 Not Found"), "Not Found");
        assert_eq!(status_text(200, "OK"), "OK");
        assert_eq!(status_text(500, ""), "");
    }
}
