//! zcall-v2: cầu nối giữa renderer và trình gọi native (`ZaloCall`).
//!
//! Port Rust của `CallBridge.swift` + `CallTransport.swift` (bản macOS):
//!
//!   renderer ──`call-send-to-native` / `call-init`──▶ Bridge ──mã hoá, cắt khúc──▶ socket send ──▶ helper
//!   renderer ◀──`call-update` / `call-send-signal` / `call-request` / `call-response-*`── socket recv ◀── helper
//!
//! Định dạng frame hai chiều: `hex(AES-128-CBC(json))` kết thúc bằng `$`
//! (payload > 4000 ký tự phía main → helper được cắt thành
//! `<khúc>#<seq>#<số khúc>#<chỉ số>#$`, một khúc trên wire, helper xác nhận
//! mỗi khúc bằng cách ghi lại bất kỳ dữ liệu gì). Trên Linux helper là binary
//! ngoài được trỏ bằng `OLAZ_CALL_HELPER` — không đặt thì mọi thứ vẫn chạy
//! graceful (tín hiệu được xếp hàng, chỉ có log).
//!
//! Nội dung message là JSON chứa token/id: chỉ log tên lệnh (chính sách chung).

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command as ProcessCommand};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};

use crate::host::Events;

/// `callV2TransportAesKey` (base64 của khoá AES 16 byte) — hằng số của client
/// chính thức (module "5NUA"), xem `CallTransport.swift`.
const KEY_BASE64: &str = "yjAF9oqMWl6XfXYJn9mA7w==";
const CHUNK_SIZE: usize = 4000;
const MAX_PENDING: usize = 500;
const MAX_FRAME_BUFFER: usize = 4_000_000;

/// Khoá transport giải mã một lần rồi cache (`transport_key()` cũ decode
/// base64 ở mọi frame — lãng phí vì khoá là hằng số của client).
fn transport_key() -> &'static [u8] {
    static KEY: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        base64::engine::general_purpose::STANDARD
            .decode(KEY_BASE64)
            .unwrap_or_default()
    })
}

fn zero_iv() -> [u8; 16] {
    [0u8; 16]
}

fn encrypt_frame(text: &str) -> Option<String> {
    let cipher = crate::bridge::cbc_run(true, transport_key(), &zero_iv(), text.as_bytes()).ok()?;
    Some(hex_encode(&cipher))
}

fn decrypt_frame(frame: &str) -> Option<Value> {
    let bytes = hex_decode(frame)?;
    if bytes.is_empty() || bytes.len() % 16 != 0 {
        return None;
    }
    let plain = crate::bridge::cbc_run(false, transport_key(), &zero_iv(), &bytes).ok()?;
    // `decrypt → strip NUL → JSON.parse` (CallFrameDecoder.parse).
    let cleaned: Vec<u8> = plain.into_iter().filter(|&byte| byte != 0).collect();
    serde_json::from_slice(&cleaned).ok()
}

const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX_CHARS[(byte >> 4) as usize] as char);
        out.push(HEX_CHARS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    let chars = hex.trim().as_bytes();
    if chars.len() % 2 != 0 {
        return None;
    }
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    chars
        .chunks(2)
        .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

/// `CallChunker`: `r` bắt đầu từ 1 và mỗi message là `max(r + 1, Date.now())`.
#[derive(Default)]
struct Chunker {
    sequence: u64,
}

impl Chunker {
    fn chunks(&mut self, payload: &str) -> Vec<String> {
        let count = payload.len().div_ceil(CHUNK_SIZE);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        self.sequence = (self.sequence + 1).max(now);
        if count <= 1 {
            return vec![format!("{payload}$")];
        }
        // `payload` là hex thường (ASCII): offset byte = offset ký tự.
        (0..count)
            .map(|index| {
                let start = index * CHUNK_SIZE;
                let end = payload.len().min(start + CHUNK_SIZE);
                format!("{}#{}#{}#{}#$", &payload[start..end], self.sequence, count, index)
            })
            .collect()
    }
}

/// Gom luồng byte helper → main theo `$`; frame chưa đủ chờ phần còn lại.
struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Trả về `(message hoàn chỉnh, số frame bỏ)`.
    fn feed(&mut self, data: &[u8]) -> (Vec<Value>, usize) {
        self.buffer.extend_from_slice(data);
        let mut messages = Vec::new();
        let mut dropped = 0;
        while let Some(end) = self.buffer.iter().position(|&byte| byte == b'$') {
            let frame: Vec<u8> = self.buffer.drain(..=end).collect();
            let text = String::from_utf8_lossy(&frame[..end]).trim().to_string();
            if text.is_empty() {
                continue;
            }
            match decrypt_frame(&text) {
                Some(message) => messages.push(message),
                None => dropped += 1,
            }
        }
        if self.buffer.len() > MAX_FRAME_BUFFER {
            self.buffer.clear();
            dropped += 1;
        }
        (messages, dropped)
    }
}

fn undefined_marker() -> Value {
    json!({ "__undefined": true })
}

enum Command {
    SendToNative { json: String, optional: bool },
    Init(Option<String>),
    Reset,
    /// Người dùng từ chối cuộc gọi đến (banner in-page): virtual mode gửi
    /// 405/12437 qua `decline_incoming`, zrtc mode qua `engine.reject`.
    Decline,
    /// Người dùng nhận cuộc gọi đến (zrtc engine mode).
    Accept,
    /// Điều khiển trong cuộc gọi (zrtc engine): {"action":"end"}…
    CallControl(Value),
    /// Kết nối mới trên socket send (bản ghi để worker ghi).
    SendUp { conn: usize, stream: UnixStream },
    /// Helper xác nhận một khúc (ghi lại bất kỳ dữ liệu gì).
    SendAck { conn: usize },
    SendDown { conn: usize },
    RecvUp { conn: usize },
    RecvDown { conn: usize },
    /// Message đã giải mã từ socket recv.
    HelperMessage(Value),
}

/// Tay cầm cho các thread khác (bridge) gửi lệnh vào worker.
#[derive(Clone)]
pub struct CallHandle {
    tx: Sender<Command>,
}

impl CallHandle {
    pub fn send_to_native(&self, json: String, optional: bool) {
        let _ = self.tx.send(Command::SendToNative { json, optional });
    }

    pub fn init(&self, json: Option<String>) {
        let _ = self.tx.send(Command::Init(json));
    }

    pub fn reset(&self) {
        let _ = self.tx.send(Command::Reset);
    }

    pub fn decline(&self) {
        let _ = self.tx.send(Command::Decline);
    }

    pub fn accept(&self) {
        let _ = self.tx.send(Command::Accept);
    }

    pub fn call_control(&self, data: Value) {
        let _ = self.tx.send(Command::CallControl(data));
    }
}

struct State {
    generation: u64,
    /// Chế độ helper: `External` (binary qua `OLAZ_CALL_HELPER`), `Virtual`
    /// (mặc định — xử lý mức UI ngay trong Rust: listDevice, từ chối gọi đến).
    mode: Mode,
    started: bool,
    helper: Option<Child>,
    unavailable_logged: bool,
    socket_dir: Option<PathBuf>,
    socket_dir_cleaned: bool,
    init_json: Option<String>,
    pending: VecDeque<String>,
    native_ready: bool,
    flushed_for_ready: bool,
    /// Kết nối hiện tại trên socket send: id + bản ghi để ghi.
    send_conn: Option<(usize, UnixStream)>,
    awaiting_ack: bool,
    chunk_queue: VecDeque<String>,
    chunker: Chunker,
    restart_after_exit: bool,
    /// `killMe`: (pid, mốc SIGTERM kế, giai đoạn) — SIGINT đã gửi trước đó.
    escalate: Option<(u32, Instant, u8)>,
    recv_generations: HashMap<usize, u64>,
    send_generations: HashMap<usize, u64>,
    // Virtual mode: danh sách thiết bị từ init data + cuộc gọi đến đang reo.
    virtual_devices: Option<Value>,
    virtual_incoming: Option<Value>,
    /// zrtc engine mode: máy trạng thái + media thật (OLAZ_CALL_ENGINE=zrtc).
    zrtc: Option<crate::zrtc::engine::Engine<crate::zrtc::media::ZrtcMedia>>,
}

impl State {
    fn new(mode: Mode) -> Self {
        Self {
            generation: 0,
            mode,
            started: false,
            helper: None,
            unavailable_logged: false,
            socket_dir: None,
            socket_dir_cleaned: false,
            init_json: None,
            pending: VecDeque::new(),
            native_ready: false,
            flushed_for_ready: false,
            send_conn: None,
            awaiting_ack: false,
            chunk_queue: VecDeque::new(),
            chunker: Chunker::default(),
            restart_after_exit: false,
            escalate: None,
            recv_generations: HashMap::new(),
            send_generations: HashMap::new(),
            virtual_devices: None,
            virtual_incoming: None,
            zrtc: None,
        }
    }
}

/// Chế độ helper.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Binary ngoài qua `OLAZ_CALL_HELPER` (khi có helper tương thích).
    External,
    /// Máy trạng thái + media engine zrtc trong Rust (`OLAZ_CALL_ENGINE=zrtc`):
    /// nhận máy được (402), audio qua cpal+Opus.
    Zrtc,
    /// Mặc định: không có binary — xử lý mức UI ngay trong Rust
    /// (trả lời `listDevice`, theo dõi cuộc gọi đến, từ chối).
    Virtual,
    /// `OLAZ_CALL_HELPER=none`: tắt hẳn.
    Off,
}

fn helper_mode() -> Mode {
    // zrtc engine (media thật) opt-in qua OLAZ_CALL_ENGINE=zrtc — ưu tiên hơn
    // helper mode; mặc định vẫn là virtual để không phá hành vi từ chối.
    if std::env::var("OLAZ_CALL_ENGINE").map(|v| v.trim() == "zrtc").unwrap_or(false) {
        return Mode::Zrtc;
    }
    match std::env::var("OLAZ_CALL_HELPER") {
        Ok(value) => match value.trim() {
            "" | "none" | "off" => Mode::Off,
            "virtual" => Mode::Virtual,
            _ => Mode::External,
        },
        Err(_) => Mode::Virtual,
    }
}

/// Khởi động worker + hai socket server. Trả về tay cầm gửi lệnh.
pub fn start(events: Arc<Events>, debug: bool) -> CallHandle {
    let (tx, rx) = mpsc::channel::<Command>();
    let worker_tx = tx.clone();
    let mode = helper_mode();
    std::thread::Builder::new()
        .name("call-bridge".into())
        .spawn(move || worker(events, debug, mode, worker_tx, rx))
        .expect("call: không tạo được thread");
    CallHandle { tx }
}

fn worker(
    events: Arc<Events>,
    debug: bool,
    mode: Mode,
    tx: Sender<Command>,
    rx: Receiver<Command>,
) {
    let mut state = State::new(mode);
    match mode {
        Mode::Virtual => log(debug, "call: helper nội bộ (virtual) — không có cửa sổ thoại, có thể từ chối cuộc gọi đến"),
        Mode::Zrtc => log(debug, "call: zrtc engine (OLAZ_CALL_ENGINE=zrtc) — nhận máy được, audio cpal+Opus"),
        Mode::External => log(debug, "call: helper ngoài (OLAZ_CALL_HELPER)"),
        Mode::Off => log(debug, "call: helper bị tắt (OLAZ_CALL_HELPER=none)"),
    }
    loop {
        match rx.recv_timeout(Duration::from_millis(400)) {
            Ok(command) => handle(&mut state, &events, debug, &tx, command),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if mode == Mode::Zrtc {
            zrtc_tick(&mut state, &events, debug);
        }
        // Helper thoát (reap zombie) + escalation sau `killMe`.
        reap_helper(&mut state, &events, debug, &tx);
        run_escalation(&mut state, debug);
    }
}

fn handle(state: &mut State, events: &Events, debug: bool, tx: &Sender<Command>, command: Command) {
    match command {
        Command::SendToNative { json, optional } => match state.mode {
            Mode::Virtual => virtual_message(state, events, debug, json),
            Mode::Zrtc => zrtc_message(state, events, debug, json),
            Mode::External => {
                if !optional {
                    start_native(state, events, debug, tx);
                }
                deliver(state, debug, json);
            }
            Mode::Off => {}
        },
        Command::Init(json) => state.init_json = json,
        Command::Reset => reset(state, events, debug),
        Command::Decline => match state.mode {
            // zrtc engine tự từ chối qua máy trạng thái (402 status 3, nhóm
            // qua 12436) — trước đây rơi vào nhánh virtual và bị bỏ lặng.
            Mode::Zrtc => {
                if let Some(engine) = &mut state.zrtc {
                    let now = now_ms();
                    engine_out(events, engine.reject(now, "user"));
                } else {
                    eprintln!("[call] từ chối: engine chưa sẵn sàng (renderer chưa gửi init)");
                }
            }
            _ => decline_incoming(state, events, debug),
        },
        Command::Accept => {
            if let Some(engine) = &mut state.zrtc {
                let now = now_ms();
                let out = engine.accept(now);
                engine_out(events, out);
            } else if state.mode == Mode::Virtual {
                // Helper nội bộ không có media nên không thể nhận máy — đây là
                // giới hạn thiết kế, log luôn để bản release cũng thấy.
                eprintln!("[call] nhận máy: cần OLAZ_CALL_ENGINE=zrtc để nhận máy thật");
            }
        }
        Command::CallControl(data) => {
            let action = data.get("action").and_then(Value::as_str).unwrap_or("");
            if let Some(engine) = &mut state.zrtc {
                let now = now_ms();
                match action {
                    "end" => engine_out(events, engine.hangup(now, "user")),
                    "mute" => log(debug, "call: mute trong cuộc gọi — port 12006 ở bước sau"),
                    _ => {}
                }
            }
        }
        Command::SendUp { conn, stream } => {
            log(debug, "call: send connection");
            state.send_generations.insert(conn, state.generation);
            state.send_conn = Some((conn, stream));
            // Kết nối mới bắt đầu sạch: không còn nửa khúc cho helper cũ.
            state.awaiting_ack = false;
            state.chunk_queue.clear();
            flush_if_ready(state, debug);
        }
        Command::SendAck { conn } => {
            let current = state.send_conn.as_ref().is_some_and(|(id, _)| *id == conn);
            if current {
                state.awaiting_ack = false;
                pump(state, debug);
            }
        }
        Command::SendDown { conn } => {
            let ended = state.send_generations.remove(&conn);
            let current = state.send_conn.as_ref().is_some_and(|(id, _)| *id == conn)
                && ended == Some(state.generation);
            if current {
                log(debug, "call: send end");
                reset_connection_state(state);
                emit(events, "call-update", &[json!("callState"), json!({ "state": "free" })]);
            }
        }
        Command::RecvUp { conn } => {
            log(debug, "call: recv connection");
            state.recv_generations.insert(conn, state.generation);
        }
        Command::RecvDown { conn } => {
            state.recv_generations.remove(&conn);
            log(debug, "call: recv end");
            // Kết nối của helper cũ không nói gì về helper hiện tại; chỉ kết nối
            // cuối cùng của helper này đi thì mới reset (như `CallBridge.swift`).
            let still_connected = state.recv_generations.values().any(|&gen| gen == state.generation);
            if !still_connected {
                reset_connection_state(state);
                emit(events, "call-update", &[json!("callState"), json!({ "state": "free" })]);
            }
        }
        Command::HelperMessage(message) => route(state, events, debug, message),
    }
}

/// `try_wait` helper: thoát thật thì phát `callState free` (+ khởi động lại khi
/// `killMe` đã yêu cầu). `try_wait` cũng reap zombie.
fn reap_helper(state: &mut State, events: &Events, debug: bool, tx: &Sender<Command>) {
    let exited = state
        .helper
        .as_mut()
        .and_then(|child| child.try_wait().ok())
        .flatten();
    if let Some(status) = exited {
        let code = status.code().unwrap_or(-1);
        log(debug, &format!("call: native exit {code}"));
        state.helper = None;
        state.started = false;
        state.escalate = None;
        reset_connection_state(state);
        emit(events, "call-update", &[json!("callState"), json!({ "state": "free" })]);
        if state.restart_after_exit {
            state.restart_after_exit = false;
            log(debug, "call: native restart");
            start_native(state, events, debug, tx);
        }
    }
}

fn run_escalation(state: &mut State, debug: bool) {
    let Some((pid, deadline, stage)) = state.escalate else {
        return;
    };
    if Instant::now() < deadline {
        return;
    }
    let still_ours = state.helper.as_ref().is_some_and(|child| child.id() == pid);
    if !still_ours {
        state.escalate = None;
        return;
    }
    let signal = if stage == 0 { libc::SIGTERM } else { libc::SIGKILL };
    unsafe {
        libc::kill(pid as i32, signal);
    }
    log(debug, &format!("call: escalate stage {stage}"));
    state.escalate = if stage == 0 {
        Some((pid, Instant::now() + Duration::from_secs(2), 1))
    } else {
        None
    };
}

fn emit(events: &Events, channel: &str, args: &[Value]) {
    events.push(json!({ "kind": "send", "channel": channel, "args": args }));
}

/// `.helperUnavailable` trên macOS hiện alert; ở đây là toast trong trang
/// (control event `kind:"js"` — cùng cơ chế với banner/picker).
fn helper_notice(events: &Events, message: &str) {
    let message = serde_json::to_string(&format!("Cuộc gọi: {message}")).unwrap_or_default();
    events.push_control(
        u64::MAX - 60,
        "main",
        json!({
            "kind": "js",
            "target": "main",
            "code": format!("window.__olazCallUI && window.__olazCallUI.toast({message});")
        }),
    );
}

/// Virtual mode: xử lý message của renderer như một helper mức UI
/// (không có socket, không có binary).
fn virtual_message(state: &mut State, events: &Events, debug: bool, json: String) {
    let Ok(message) = serde_json::from_str::<Value>(&json) else {
        log(debug, "call: không parse được message từ renderer");
        return;
    };
    let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
    let command = message.get("command").and_then(Value::as_str).unwrap_or("");
    match (kind, command) {
        // Helper nội bộ không đặt cuộc gọi được: log rõ thay vì treo lặng
        // (adapter báo lạc quan `started`, renderer thì chờ response/show).
        ("request", "makeCall" | "endCall") => {
            eprintln!("[call] gọi đi: helper nội bộ chỉ từ chối — bật OLAZ_CALL_ENGINE=zrtc để gọi/nhận máy thật");
        }
        // init data chứa `_getDefaultDevices()` — giữ lại để trả `listDevice`.
        ("update", "init" | "updateLocal") => {
            if let Some(data) = message.get("data") {
                state.virtual_devices = Some(data.clone());
            }
        }
        // Settings page hỏi danh sách thiết bị (chờ tối đa 25s nếu không trả).
        ("request", "listDevice") => {
            let data = state.virtual_devices.clone().unwrap_or_else(|| {
                json!({
                    "autoAudioInput": true,
                    "autoAudioOutput": true,
                    "defaultAudInputDevice": null,
                    "defaultAudOutputDevice": null,
                    "defaultVidDevice": null,
                })
            });
            emit(events, "call-response-listDevice", &[data]);
        }
        ("control", _) => virtual_control(state, events, debug, message),
        _ => {}
    }
}

/// Control frame voip renderer relay cho helper (`isIncomingCallEvent` =
/// `request` / `group_request`): ghi lại để banner có thể từ chối.
fn virtual_control(state: &mut State, events: &Events, debug: bool, message: Value) {
    let Some(control) = message.get("data").cloned() else {
        return;
    };
    let act = control.get("act").and_then(Value::as_str).unwrap_or("");
    match act {
        "request" | "group_request" => {
            let Some(data) = control.get("data") else {
                return;
            };
            // Đang reo/bận cuộc khác: retransmit cùng callId thì bỏ qua,
            // cuộc mới khác id thì auto-decline, giữ nguyên cuộc đang reo.
            if let Some(current) = &state.virtual_incoming {
                let same = match (
                    current.get("data").and_then(|d| d.get("callId")).filter(|v| !v.is_null()),
                    data.get("callId").filter(|v| !v.is_null()),
                ) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                };
                if same {
                    return;
                }
                match virtual_decline_payload(&control) {
                    Some((code, payload)) => {
                        emit(events, "call-send-signal", &[json!(code), payload]);
                        eprintln!("[call] đang bận — đã tự từ chối cuộc gọi mới");
                    }
                    None => eprintln!("[call] đang bận nhưng control mới thiếu callId — không từ chối được"),
                }
                return;
            }
            state.virtual_incoming = Some(control.clone());
            // Helper thật cũng reo: renderer coi như đang trong cuộc gọi.
            emit(events, "call-update", &[json!("callState"), json!({ "state": "incall" })]);
            log(
                debug,
                &format!(
                    "call: cuộc gọi đến đang reo ({}) — từ chối qua channel call-decline",
                    if act == "group_request" { "nhóm" } else { "1-1" }
                ),
            );
            let _ = data;
        }
        // Người gọi huỷ trước khi mình trả lời.
        "cancel" | "group_cancel" => {
            if state.virtual_incoming.take().is_some() {
                emit(events, "call-update", &[json!("callState"), json!({ "state": "free" })]);
                log(debug, "call: người gọi đã huỷ");
            }
        }
        _ => {}
    }
}

/// Dựng signal từ chối cho một control request mà KHÔNG chạm state: dùng cho
/// từ chối cuộc đang reo lẫn auto-decline khi bận (không emit `free` — cuộc
/// hiện tại vẫn sống, chỉ báo cho server biết cuộc mới bị từ chối).
fn virtual_decline_payload(control: &Value) -> Option<(i64, Value)> {
    let data = control.get("data")?;
    let call_id = data.get("callId").filter(|value| !value.is_null())?.clone();
    let to_id = first_string(data, &["uidN", "fromUid", "uid"]);
    let call_type = data
        .get("callType")
        .cloned()
        .or_else(|| data.get("type").cloned())
        .unwrap_or(Value::Null);
    let is_group = control.get("act").and_then(Value::as_str) == Some("group_request")
        || to_id.starts_with('g');
    Some(if is_group {
        (
            12437,
            json!({
                "calleeId": to_id,
                "callId": call_id,
                "callType": call_type,
                "hostCall": data.get("hostCall").cloned().unwrap_or(Value::Null),
                "data": data.get("data").cloned().unwrap_or(Value::Null),
                "partners": data.get("partners").cloned().unwrap_or(Value::Null),
            }),
        )
    } else {
        (
            405,
            json!({ "toId": to_id, "callId": call_id, "callType": call_type }),
        )
    })
}

/// Từ chối cuộc gọi đến đang reo: gửi signal 405 (1-1) / 12437 (nhóm) —
/// renderer relay lên server qua realtime (`sendCancelCall` /
/// `sendCancelCallGroup`), đúng như helper thật làm.
fn decline_incoming(state: &mut State, events: &Events, debug: bool) {
    if state.mode != Mode::Virtual {
        // Helper ngoài tự vẽ cửa sổ thoại và tự xử lý; zrtc mode đi nhánh
        // engine riêng. Log luôn để bản release cũng thấy nút chết ở đâu.
        eprintln!("[call] từ chối: chỉ dùng với helper nội bộ (mode hiện tại không phải virtual)");
        return;
    }
    let Some(control) = state.virtual_incoming.take() else {
        eprintln!("[call] từ chối: không có cuộc gọi đến đang reo (control request chưa tới)");
        return;
    };
    match virtual_decline_payload(&control) {
        Some((code, payload)) => {
            emit(events, "call-send-signal", &[json!(code), payload]);
            emit(events, "call-update", &[json!("callState"), json!({ "state": "free" })]);
            log(debug, "call: đã từ chối cuộc gọi đến");
        }
        None => {
            if control.get("data").is_none() {
                eprintln!("[call] từ chối: control request thiếu data");
            } else {
                eprintln!("[call] từ chối: control request thiếu callId — không gửi được 405");
            }
        }
    }
}

/// zrtc engine mode: xử lý message renderer (init/makeCall/control) qua engine.
fn zrtc_message(state: &mut State, events: &Events, debug: bool, json: String) {
    let Ok(message) = serde_json::from_str::<Value>(&json) else {
        log(debug, "call: không parse được message từ renderer");
        return;
    };
    let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
    let command = message.get("command").and_then(Value::as_str).unwrap_or("");
    let now = now_ms();
    // Engine được tạo ở init đầu tiên (cần local id/avatar cho 401/extendData).
    if state.zrtc.is_none() && kind == "update" && matches!(command, "init" | "updateLocal") {
        let local = message.get("data").cloned().unwrap_or(json!({}));
        state.zrtc = Some(crate::zrtc::engine::Engine::new(crate::zrtc::media::ZrtcMedia::spawn(), local));
        log(debug, "call: zrtc engine sẵn sàng");
    }
    let Some(engine) = &mut state.zrtc else { return };
    match (kind, command) {
        ("update", "init" | "updateLocal") => {
            // init data mới (đổi máy/device): local đã nạp; giữ listDevice như virtual.
            if let Some(data) = message.get("data") {
                state.virtual_devices = Some(data.clone());
            }
        }
        ("request", "makeCall") => {
            let out = engine.make_call(now, &message.get("data").cloned().unwrap_or(Value::Null));
            engine_out(events, out);
        }
        ("request", "endCall") => {
            let out = engine.hangup(now, "zalo");
            engine_out(events, out);
        }
        ("control", _) => {
            let Some(control) = message.get("data") else { return };
            let act = control.get("act").and_then(Value::as_str).unwrap_or("");
            let out = engine.handle_control(now, act, control);
            engine_out(events, out);
        }
        ("recvSignal", _) => {
            // Relay trả lời signal của ta (servers/sessId của 401, session của
            // 12433, ping 12097): engine xử lý — trước đây rơi vào `_` nên
            // gọi đi/group liệt hoàn toàn ở zrtc mode.
            let code = match message.get("command") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Number(number)) => number.to_string(),
                _ => String::new(),
            };
            let out = engine.handle_recv_signal(
                now,
                &code,
                &message.get("data").cloned().unwrap_or(Value::Null),
            );
            engine_out(events, out);
        }
        ("response", "getAliasName") => {
            // Alias hiển thị — phase sau; signalling không phụ thuộc.
        }
        _ => {}
    }
}

/// Tick worker: media events (registration done, peer audio, fault) + deadline.
fn zrtc_tick(state: &mut State, events: &Events, debug: bool) {
    let Some(engine) = &mut state.zrtc else { return };
    let now = now_ms();
    for event in engine.media.drain_events() {
        let out = match event {
            crate::zrtc::media::MediaEvent::Prepared(best) => engine.media_prepared(now, best),
            crate::zrtc::media::MediaEvent::EarlyAudio(phase) => engine.early_audio(now, phase),
            crate::zrtc::media::MediaEvent::GroupRoster(roster) => engine.group_roster(now, &roster),
            crate::zrtc::media::MediaEvent::Fault(kind) => {
                // Hỏng thiết bị giữa cuộc gọi: call tiếp tục (bên kia vẫn nghe
                // nếu chỉ hỏng loa), nhưng phải báo cho người dùng biết.
                log(debug, &format!("call: audio fault ({kind}) — call vẫn tiếp tục, đã báo UI"));
                vec![crate::zrtc::engine::Out::Ui(crate::zrtc::engine::UiEvent::Status(
                    fault_status(kind),
                ))]
            }
            crate::zrtc::media::MediaEvent::RemoteVideo { jpeg } => {
                // Ảnh bên kia (~2 fps) cho panel in-call của call-ui.js.
                let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
                emit(events, "call-video", &[json!({"jpeg": b64})]);
                vec![]
            }
        };
        engine_out(events, out);
    }
    let out = engine.tick(now);
    engine_out(events, out);
}

/// Lỗi thiết bị giữa cuộc gọi → mã trạng thái cho panel in-call.
fn fault_status(kind: &str) -> String {
    match kind {
        "mic" => "micUnavailable".into(),
        "speaker" => "speakerUnavailable".into(),
        _ => "audioUnavailable".into(),
    }
}

/// Map engine events onto the existing bridge channels.
fn engine_out(events: &Events, out: Vec<crate::zrtc::engine::Out>) {
    for event in out {
        match event {
            crate::zrtc::engine::Out::SendSignal { code, data } => {
                emit(events, "call-send-signal", &[json!(code), data]);
            }
            crate::zrtc::engine::Out::Update { command, data } => {
                // Panel in-call của call-ui.js (channel riêng, renderer không ăn).
                if command == "callState" {
                    emit(events, "call-ui", &[data.clone()]);
                }
                emit(events, "call-update", &[json!(command), data]);
            }
            crate::zrtc::engine::Out::Request { command, data } => {
                emit(events, "call-request", &[json!(command), data]);
            }
            crate::zrtc::engine::Out::Response { command, data } => {
                emit(events, &format!("call-response-{command}"), &[data]);
            }
            crate::zrtc::engine::Out::Ui(ui) => {
                // Banner cuộc gọi đến do chính renderer vẽ (nó relay control);
                // trạng thái trong cuộc gọi đi qua callState phía trên.
                if let crate::zrtc::engine::UiEvent::Status(code) = ui {
                    emit(events, "call-ui", &[json!({"status": code})]);
                }
            }
        }
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

fn first_string(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        match value.get(key) {
            Some(Value::String(text)) if !text.is_empty() => return text.clone(),
            Some(Value::Number(number)) => return number.to_string(),
            _ => {}
        }
    }
    String::new()
}

fn log(debug: bool, message: &str) {
    if debug {
        eprintln!("{message}");
    }
}

/// `K()`: tạo socket dir, spawn helper. Không tìm thấy helper vẫn đánh dấu
/// `started` (message được xếp hàng, không dồn log).
fn start_native(state: &mut State, events: &Events, debug: bool, tx: &Sender<Command>) {
    if state.started {
        log(debug, "call: native already started");
        return;
    }
    let helper_path = std::env::var_os("OLAZ_CALL_HELPER").map(PathBuf::from);
    let executable = helper_path.filter(|path| std::fs::metadata(path).is_ok());
    let Some(executable) = executable else {
        // Mode::External đòi path tồn tại; tới đây nghĩa là file biến mất.
        if !state.unavailable_logged {
            state.unavailable_logged = true;
            eprintln!("[call] helper không tìm thấy hoặc không đọc được — cuộc gọi sẽ không có cửa sổ thoại");
            // Như `.helperUnavailable` trên macOS (alert): báo cho người dùng.
            helper_notice(events, "Không tìm thấy trình gọi của Zalo — cuộc gọi sẽ không có cửa sổ thoại.");
        }
        state.started = true;
        state.generation += 1;
        let _ = prepare_sockets(state, debug, tx);
        return;
    };
    if !state.socket_dir_cleaned {
        state.socket_dir_cleaned = true;
        remove_stale_directories();
    }
    state.started = true;
    state.generation += 1;
    if let Err(error) = prepare_sockets(state, debug, tx) {
        state.started = false;
        eprintln!("[call] không tạo được socket: {error}");
        return;
    }
    let Some(dir) = &state.socket_dir else {
        state.started = false;
        return;
    };
    let recv_path = dir.join("recv");
    let send_path = dir.join("send");
    let mut command = ProcessCommand::new(&executable);
    command
        .arg(&recv_path)
        .arg(&send_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Helper chết theo app: Linux tương đương `shutdown()` + `CallOrphans.reap`
    // của bản macOS — SIGTERM khi tiến trình cha thoát, không bao giờ bỏ lại
    // một ZaloCall mồ côi giữ socket đã chết.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    match command.spawn() {
        Ok(child) => {
            state.helper = Some(child);
            state.unavailable_logged = false;
            log(debug, "call: native started");
        }
        Err(error) => {
            state.started = false;
            eprintln!("[call] không khởi động được trình gọi: {error}");
            helper_notice(events, "Không khởi động được trình gọi của Zalo.");
        }
    }
}

fn reset(state: &mut State, events: &Events, debug: bool) {
    // zrtc engine: logout/reset → dừng media + xoá máy trạng thái (lần init
    // sau sẽ tạo lại).
    state.zrtc = None;
    state.restart_after_exit = false;
    state.init_json = None;
    state.pending.clear();
    state.virtual_devices = None;
    state.virtual_incoming = None;
    // Đang trong cuộc gọi mà reset (logout): renderer phải về free, nếu không
    // banner/panel treo (kèm hideBanner qua call-update ở call-ui.js).
    emit(events, "call-update", &[json!("callState"), json!({ "state": "free" })]);
    state.generation += 1;
    if let Some(mut child) = state.helper.take() {
        state.started = false;
        let _ = child.kill();
    }
    reset_connection_state(state);
    log(debug, "call: reset");
}

fn reset_connection_state(state: &mut State) {
    state.send_conn = None;
    state.native_ready = false;
    state.flushed_for_ready = false;
    state.awaiting_ack = false;
    state.chunk_queue.clear();
}

fn prepare_sockets(state: &mut State, debug: bool, tx: &Sender<Command>) -> Result<(), String> {
    if state.socket_dir.is_some() {
        return Ok(());
    }
    let dir = make_socket_directory()?;
    let recv_path = dir.join("recv");
    let send_path = dir.join("send");
    let recv_listener = UnixListener::bind(&recv_path).map_err(|error| error.to_string())?;
    let send_listener = UnixListener::bind(&send_path).map_err(|error| error.to_string())?;
    set_private(&dir);
    state.socket_dir = Some(dir);
    log(debug, "call: sockets ready");

    // socket send: chấp nhận kết nối, đưa bản ghi về worker để ghi, đọc ack.
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("call-send-accept".into())
            .spawn(move || {
                for stream in send_listener.incoming() {
                    let Ok(stream) = stream else { break };
                    let conn = NEXT_CONN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if let Ok(write_half) = stream.try_clone() {
                        let _ = tx.send(Command::SendUp { conn, stream: write_half });
                    }
                    let tx = tx.clone();
                    std::thread::Builder::new()
                        .name("call-send-ack".into())
                        .spawn(move || {
                            let mut reader = stream;
                            let mut buffer = [0u8; 64];
                            loop {
                                match reader.read(&mut buffer) {
                                    Ok(0) | Err(_) => {
                                        let _ = tx.send(Command::SendDown { conn });
                                        break;
                                    }
                                    Ok(_) => {
                                        let _ = tx.send(Command::SendAck { conn });
                                    }
                                }
                            }
                        })
                        .ok();
                }
            })
            .ok();
    }

    // socket recv: mỗi kết nối một thread giải mã frame.
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("call-recv-accept".into())
            .spawn(move || {
                for stream in recv_listener.incoming() {
                    let Ok(stream) = stream else { break };
                    let conn = NEXT_CONN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = tx.send(Command::RecvUp { conn });
                    let tx = tx.clone();
                    std::thread::Builder::new()
                        .name("call-recv-read".into())
                        .spawn(move || {
                            let mut reader = stream;
                            let mut decoder = FrameDecoder::new();
                            let mut buffer = [0u8; 8192];
                            loop {
                                match reader.read(&mut buffer) {
                                    Ok(0) | Err(_) => {
                                        let _ = tx.send(Command::RecvDown { conn });
                                        break;
                                    }
                                    Ok(count) => {
                                        let (messages, dropped) = decoder.feed(&buffer[..count]);
                                        if dropped > 0 {
                                            eprintln!("[call] cannot parse payload ×{dropped}");
                                        }
                                        for message in messages {
                                            let _ = tx.send(Command::HelperMessage(message));
                                        }
                                    }
                                }
                            }
                        })
                        .ok();
                }
            })
            .ok();
    }
    Ok(())
}

/// Số kết nối tăng dần cho id socket (dùng chung hai chiều).
static NEXT_CONN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// Thư mục 0700 duy nhất qua `mkdtemp` (`/tmp/olaz-call-<pid>-XXXXXX`).
fn make_socket_directory() -> Result<PathBuf, String> {
    let template = format!("/tmp/olaz-call-{}-XXXXXX\0", std::process::id());
    let mut bytes = template.into_bytes();
    let pointer = unsafe { libc::mkdtemp(bytes.as_mut_ptr() as *mut libc::c_char) };
    if pointer.is_null() {
        return Err("mkdtemp thất bại".into());
    }
    let end = bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len());
    Ok(PathBuf::from(String::from_utf8_lossy(&bytes[..end]).to_string()))
}

fn set_private(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

/// Thư mục socket của tiến trình Olaz đã chết (crash / force quit).
fn remove_stale_directories() {
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return;
    };
    let prefix = "olaz-call-";
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(prefix) || !entry.path().is_dir() {
            continue;
        }
        let Some(pid) = name[prefix.len()..]
            .split('-')
            .next()
            .and_then(|part| part.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() || process_alive(pid) {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

/// `kill(pid, 0)`: tiến trình còn sống trừ khi gặp ESRCH.
fn process_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// `O`: trước khi helper nối vào socket send, message chờ (và đi ra khi
/// `native-ready`, sau init data); đã nối thì gửi ngay.
fn deliver(state: &mut State, debug: bool, json: String) {
    if state.send_conn.is_some() {
        transmit(state, debug, json);
    } else {
        state.pending.push_back(json);
        while state.pending.len() > MAX_PENDING {
            state.pending.pop_front();
        }
    }
}

/// `G(e, t)`: mã hoá, cắt khúc, xếp hàng, gửi một khúc.
fn transmit(state: &mut State, debug: bool, json: String) {
    let Some(hex) = encrypt_frame(&json) else {
        log(debug, "call: encrypt failed");
        return;
    };
    state.chunk_queue.extend(state.chunker.chunks(&hex));
    pump(state, debug);
}

/// `W(e)`: mỗi lần một khúc; khúc kế đi ra khi helper ghi gì đó lại.
fn pump(state: &mut State, debug: bool) {
    let pending = match (&state.send_conn, state.awaiting_ack, state.chunk_queue.front()) {
        (Some((_, stream)), false, Some(chunk)) => Some((stream, chunk.clone())),
        _ => None,
    };
    if let Some((mut stream, chunk)) = pending {
        state.chunk_queue.pop_front();
        state.awaiting_ack = true;
        if let Err(error) = stream.write_all(chunk.as_bytes()) {
            log(debug, &format!("call: ghi khúc lỗi: {error}"));
        } else {
            let _ = stream.flush();
        }
    }
}

/// `native-ready`: gửi init data trước, rồi mọi thứ đang chờ.
fn flush_if_ready(state: &mut State, debug: bool) {
    if !state.native_ready || state.send_conn.is_none() || state.flushed_for_ready {
        return;
    }
    state.flushed_for_ready = true;
    if let Some(init) = state.init_json.take() {
        log(debug, "call: sending init data");
        transmit(state, debug, init);
    }
    let queued: Vec<String> = state.pending.drain(..).collect();
    for json in queued {
        transmit(state, debug, json);
    }
}

/// `Y` / `z`: định tuyến message helper → renderer.
fn route(state: &mut State, events: &Events, debug: bool, message: Value) {
    let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
    // `command` là string với phần lớn message nhưng là số cho `sendSignal`
    // (401, 402, …): truyền đi nguyên trạng.
    let raw_command = message.get("command").cloned();
    let command_str = raw_command.as_ref().and_then(Value::as_str);
    let data = message.get("data").cloned().unwrap_or_else(undefined_marker);
    let name = raw_command.clone().unwrap_or_else(undefined_marker);
    let command_name = match &raw_command {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => "-".into(),
    };
    log(debug, &format!("call: native → {kind}/{command_name}"));
    match kind {
        "update" => {
            if command_str == Some("native-ready") {
                state.native_ready = true;
                state.flushed_for_ready = false;
                flush_if_ready(state, debug);
            } else {
                emit(events, "call-update", &[name, data]);
            }
        }
        "sendSignal" => emit(events, "call-send-signal", &[name, data]),
        "request" => {
            if command_str == Some("killMe") {
                let Some(child) = &state.helper else { return };
                let pid = child.id();
                state.restart_after_exit = true;
                unsafe {
                    libc::kill(pid as i32, libc::SIGINT);
                }
                log(debug, "call: kill native");
                // SIGINT là lời đề lịch sự; đảm bảo helper thật sự đi (và khởi động lại).
                state.escalate = Some((pid, Instant::now() + Duration::from_secs(3), 0));
            } else {
                emit(events, "call-request", &[name, data]);
            }
        }
        "response" => {
            let channel = format!(
                "call-response-{}",
                match &raw_command {
                    Some(Value::String(text)) => text.clone(),
                    Some(other) => other.to_string(),
                    None => "undefined".into(),
                }
            );
            emit(events, &channel, &[data]);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn drive(state: &mut State, events: &Arc<Events>, command: Command) {
        let (tx, _rx) = mpsc::channel::<Command>();
        handle(state, events, false, &tx, command);
    }

    fn fresh_events() -> Arc<Events> {
        Arc::new(Events::new())
    }

    fn drain(events: &Arc<Events>) -> Vec<Value> {
        events.drain_all()
    }

    fn find_send(events_list: &[Value], code: i64) -> Option<Value> {
        events_list.iter().find_map(|event| {
            let channel = event.get("channel")?.as_str()?;
            if channel != "call-send-signal" {
                return None;
            }
            let args = event.get("args")?.as_array()?;
            if args.first()?.as_i64() == Some(code) {
                Some(args.get(1).cloned().unwrap_or(Value::Null))
            } else {
                None
            }
        })
    }

    /// Khung control `request` như renderer gửi khi có cuộc gọi đến
    /// (phone → PC): `{type:"control", data:{act, data:{uidN, callId…}}}`.
    fn incoming_request_json() -> String {
        json!({
            "type": "control",
            "data": {
                "act_type": "voip",
                "act": "request",
                "data": {
                    "uidN": "123456",
                    "callId": 78901234,
                    "callType": 1,
                    "params": {"Dname": "Mẹ", "avatar": "", "sessId": "sess-1"},
                },
                "ts": 1_700_000_000_000u64,
            },
        })
        .to_string()
    }

    #[test]
    fn virtual_decline_sends_405_with_call_id() {
        let events = fresh_events();
        let mut state = State::new(Mode::Virtual);
        drive(&mut state, &events, Command::SendToNative { json: incoming_request_json(), optional: false });
        drive(&mut state, &events, Command::Decline);
        let got = drain(&events);
        let payload = find_send(&got, 405).expect("thiếu signal 405 từ chối");
        assert_eq!(payload.get("toId").and_then(Value::as_str), Some("123456"));
        assert_eq!(payload.get("callId").and_then(Value::as_i64), Some(78901234));
        // Trạng thái renderer về free sau khi từ chối.
        assert!(got.iter().any(|event| event.get("channel").and_then(Value::as_str) == Some("call-update")
            && event.get("args").and_then(|a| a.as_array()).is_some_and(|a| a.first().and_then(Value::as_str) == Some("callState"))));
    }

    #[test]
    fn zrtc_decline_sends_402_status_3() {
        let events = fresh_events();
        let mut state = State::new(Mode::Zrtc);
        // init đầu tiên dựng engine (cần local id cho 401/extendData).
        drive(
            &mut state,
            &events,
            Command::SendToNative {
                json: json!({"type": "update", "command": "init", "data": {"id": 1, "avatar": "me"}}).to_string(),
                optional: false,
            },
        );
        assert!(state.zrtc.is_some(), "engine phải được dựng sau init");
        drive(&mut state, &events, Command::SendToNative { json: incoming_request_json(), optional: false });
        drive(&mut state, &events, Command::Decline);
        let got = drain(&events);
        let payload = find_send(&got, 402).expect("thiếu signal 402 từ chối (zrtc)");
        assert_eq!(payload.get("status").and_then(Value::as_i64), Some(3));
        assert_eq!(payload.get("callId").and_then(Value::as_i64), Some(78901234));
    }

    #[test]
    fn virtual_accept_without_engine_emits_nothing() {
        // Helper nội bộ không có media: nhận máy là no-op có log (không treo,
        // không gửi signal nửa vời).
        let events = fresh_events();
        let mut state = State::new(Mode::Virtual);
        drive(&mut state, &events, Command::Accept);
        assert!(drain(&events).is_empty());
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use serde_json::json;

    fn drive(state: &mut State, events: &Arc<Events>, command: Command) {
        let (tx, _rx) = mpsc::channel::<Command>();
        handle(state, events, false, &tx, command);
    }

    fn request_with(call_id: i64, uid: &str) -> String {
        json!({
            "type": "control",
            "data": {
                "act_type": "voip",
                "act": "request",
                "data": {"uidN": uid, "callId": call_id, "callType": 1, "params": {}},
                "ts": 1_700_000_000_000u64,
            },
        })
        .to_string()
    }

    fn find_send(events_list: &[Value], code: i64) -> Option<Value> {
        events_list.iter().find_map(|event| {
            if event.get("channel")?.as_str()? != "call-send-signal" {
                return None;
            }
            let args = event.get("args")?.as_array()?;
            if args.first()?.as_i64() == Some(code) {
                Some(args.get(1).cloned().unwrap_or(Value::Null))
            } else {
                None
            }
        })
    }

    /// Gọi đi ở zrtc mode: relay trả 401 (command SỐ, đúng renderer gửi) qua
    /// recvSignal phải tới được engine — trước đây rơi vào `_` → kẹt Calling.
    #[test]
    fn zrtc_recv_signal_numeric_401_reaches_engine() {
        let events = Arc::new(Events::new());
        let mut state = State::new(Mode::Zrtc);
        drive(&mut state, &events, Command::SendToNative {
            json: json!({"type": "update", "command": "init", "data": {"id": 1}}).to_string(),
            optional: false,
        });
        drive(&mut state, &events, Command::SendToNative {
            json: json!({"type": "request", "command": "makeCall",
                         "data": {"type": 1, "partner": [{"id": "5678", "name": "Lan"}]}}).to_string(),
            optional: false,
        });
        let got = events.drain_all();
        assert!(find_send(&got, 401).is_some(), "thiếu signal 401 gọi đi");
        // Relay trả servers/sessId với command là số 401 (như renderer gửi).
        drive(&mut state, &events, Command::SendToNative {
            json: json!({"type": "recvSignal", "command": 401, "data": {
                "servers": [{"rtpaddr": "203.0.113.9|5000", "rtcpaddr": "203.0.113.9|5001"}],
                "sessId": "sess-1", "fromId": 1, "toId": 5678}}).to_string(),
            optional: false,
        });
        let call = state.zrtc.as_ref().and_then(|e| e.call.as_ref()).expect("engine mất call");
        assert!(
            call.server.as_array().map(|a| !a.is_empty()).unwrap_or(false),
            "engine chưa nạp servers từ recvSignal 401"
        );
    }

    /// Virtual mode đang reo: retransmit cùng callId thì bỏ qua, cuộc mới
    /// khác id thì auto-decline 405, cuộc cũ còn nguyên để từ chối sau.
    #[test]
    fn virtual_busy_auto_declines_only_new_call() {
        let events = Arc::new(Events::new());
        let mut state = State::new(Mode::Virtual);
        drive(&mut state, &events, Command::SendToNative { json: request_with(111, "123"), optional: false });
        drive(&mut state, &events, Command::SendToNative { json: request_with(111, "123"), optional: false });
        assert!(find_send(&events.drain_all(), 405).is_none(), "retransmit không được phát signal");
        drive(&mut state, &events, Command::SendToNative { json: request_with(222, "999"), optional: false });
        let got = events.drain_all();
        let busy = find_send(&got, 405).expect("thiếu 405 auto-decline cuộc mới");
        assert_eq!(busy.get("callId").and_then(Value::as_i64), Some(222));
        // Cuộc đầu vẫn còn: từ chối ra đúng 405 của nó.
        drive(&mut state, &events, Command::Decline);
        let got = events.drain_all();
        assert_eq!(find_send(&got, 405).expect("mất 405 cuộc đầu").get("callId").and_then(Value::as_i64), Some(111));
    }

    #[test]
    fn fault_status_maps_device_direction() {
        assert_eq!(fault_status("mic"), "micUnavailable");
        assert_eq!(fault_status("speaker"), "speakerUnavailable");
        assert_eq!(fault_status("other"), "audioUnavailable");
    }
}
