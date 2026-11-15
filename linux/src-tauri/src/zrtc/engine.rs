//! The 1-1 voice call state machine — a faithful port of the 1-1 paths of the
//! Zalos client's `engines/zcall-cpp/engine/src/engine_core.cpp` (BSD-3-Clause).
//!
//! Design for testability and for the "never block an HTTP thread" rule:
//!
//! * Every entry point takes the wall clock (`now_ms`) and returns the events
//!   it produced (`Vec<Out>`) — no hidden timers, no callbacks. The host
//!   (call.rs, C1.6) drives it from its own worker thread and maps the events
//!   onto the existing zcall-v2 bridge (`call-send-signal`, `call-update`…).
//! * The media backend sits behind a trait; the real UDP+SRTP+Opus engine
//!   lands in C1.5, tests use a fake.
//! * The call deadline (setup/ring timeout) is a value on the call; the host
//!   polls `tick(now_ms)` and fires it.
//!
//! Signalling codes (all carried by the renderer's SignallingRelay):
//! 401 request · 416 request+media · 407 ring-ring · 402 answer (status 0 =
//! accept, 3 = decline) · 408 answer-ack · 405 cancel · 409 end · 406 log.

use serde_json::{json, Map, Value};
use std::collections::VecDeque;

/// The codec list of the initial 401 is literally empty; the media-bearing
/// 416/402 carry the Opus offer.
pub const CODEC_VOICE: &str = "[{\"dynamicFptime\":0,\"frmPtime\":20,\"name\":\"opus/16000/1\",\"payload\":112}]\n";
pub const CODEC_EMPTY: &str = "[]\n";

const CALL_SETUP_TIMEOUT_MS: f64 = 15_000.0;
const RING_TIMEOUT_MS: f64 = 45_000.0;
const ENDED_CALL_TTL_MS: f64 = 60_000.0;
const MAX_ENDED_CALLS: usize = 256;

/// One outbound frame for the host to translate onto the bridge.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// `call-send-signal [code, payload]` — the renderer relays it to Zalo.
    SendSignal { code: i64, data: Value },
    /// `call-update (command, data)` — helper → renderer status updates.
    Update { command: String, data: Value },
    /// `call-request (command, data)` — e.g. `getAliasName`.
    Request { command: String, data: Value },
    /// `call-response-<command>` — answers a renderer request.
    Response { command: String, data: Value },
    /// The call-window equivalent (macOS gets this from the native ZaloCall).
    Ui(UiEvent),
}

impl UiEvent {
    pub(crate) fn status_code(&self) -> &str {
        match self {
            UiEvent::Open { status_code, .. } | UiEvent::Incoming { status_code, .. } => status_code,
            UiEvent::Status(s) => s,
            UiEvent::Close { code } => code,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    Open { title: String, status_code: String, avatar: String, video: bool, self_avatar: String },
    Incoming { title: String, status_code: String, avatar: String, video: bool, self_avatar: String },
    Status(String),
    Close { code: String },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    Caller,
    Callee,
}

impl Role {
    fn as_bool(self) -> bool {
        self == Role::Caller // "role": caller = true in the 406 payload
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    Calling,
    Ringing,
    Incoming,
    Connecting,
    Connected,
    Failed,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Deadline {
    Setup,
    Ring,
}

/// The 1-1 call record (the fields of the original `Call` this phase uses).
#[derive(Debug, Clone)]
pub struct Call {
    pub role: Role,
    pub state: State,
    pub peer_id: String,
    pub peer_uid: f64,
    pub local_uid: f64,
    pub peer_name: String,
    pub avatar: String,
    /// 1 = voice, 3 = video (video media arrives in C2; signalling is complete).
    pub call_type: f64,
    pub call_id: f64,
    pub sess_id: Value,
    pub server: Value,
    pub config: Value,
    pub p2p: Value,
    pub started_at: f64,
    pub deadline: Option<(f64, Deadline)>,
    pub muted: bool,
    // ---- group call (C3) ----
    pub group: bool,
    pub host_call: f64,
    pub group_id_str: String,
    pub group_id: f64,
    pub title: String,
    pub max_users: f64,
    pub invited: Vec<String>,
    pub invite_partners: Option<Vec<(String, String, String)>>, // (name, avatar, id)
    pub members: Vec<(MemberKey, Member)>,
    pub ping_ms: f64,
    pub answered: bool,
    pub ping_deadline: Option<f64>,
    pub greeted: Vec<f64>,
    pub members_key: String,
    pub has_status_text: bool,
    pub status_text: String,
    pub camera_on: Option<bool>,
    pub ring_deadline: Option<(f64, f64)>, // (at, ms) group no-answer timer
}

/// Group member identity: numeric uid once known, else the invite key string.
#[derive(Debug, Clone, PartialEq)]
pub enum MemberKey {
    Uid(f64),
    Str(String),
}

impl MemberKey {
    pub fn of_uid(uid: f64) -> Self {
        MemberKey::Uid(uid)
    }

    pub fn of_str(s: String) -> Self {
        MemberKey::Str(s)
    }

    /// The JSON shape the UI list uses (`src`).
    pub fn to_json(&self) -> Value {
        match self {
            MemberKey::Uid(u) => num_json(*u),
            MemberKey::Str(s) => json!(s),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Member {
    pub name: String,
    pub avatar: String,
    /// "calling" | "ringing" | "incall" | "left"
    pub state: String,
    pub muted: bool,
    pub cam_off: bool,
}

#[derive(Debug, Clone, Default)]
pub struct MemberInfo {
    pub state: Option<String>,
    pub muted: Option<bool>,
    pub cam_off: Option<bool>,
}

/// What the media backend must do when the engine tells it to. The real
/// implementation (C1.5) owns the UDP socket, SRTP and Opus; completion of the
/// relay registration comes back through [`Engine::media_prepared`].
pub trait Media {
    fn on_negotiated(&mut self, params: &NegotiatedParams);
    fn prepare(&mut self, role: Role);
    fn start(&mut self, role: Role, rtp_ser_ip: &str);
    fn stop(&mut self);
    /// Whether the backend may actually send UDP (observe-only builds do not).
    fn send_udp(&self) -> bool;
    fn stats(&self) -> (u64, u64);
}

#[derive(Debug, Clone)]
pub struct NegotiatedParams {
    pub muted: bool,
    pub speaker_off: bool,
    pub servers: Value,
    pub config: Value,
    pub srtp_mode: Value,
    pub sess_id: String,
    pub call_id: f64,
    pub role: Role,
    pub local_uid: f64,
    pub peer_uid: f64,
    pub p2p: Value,
    /// Group-call media: group init layouts, ZaviPing, RoomUpdated roster.
    pub group: bool,
    /// Group host (the caller that created the room).
    pub host: bool,
    /// Host uid (group init layout needs it).
    pub host_call: f64,
}

/// The relay registration outcome for the chosen server (`None` = no server
/// answered in time).
#[derive(Debug, Clone)]
pub struct InitReply {
    pub server: Value,
    pub public_addr: Option<String>,
}

/// JS `Number` semantics for ids: integers stay integers on the wire.
pub(crate) fn num_json(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

/// JS `String(number)`: integers without the `.0`.
pub(crate) fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

impl Call {
    /// Baseline for the group builders (`..Call::voice_template()`).
    pub(crate) fn voice_template() -> Self {
        Self {
            role: Role::Caller,
            state: State::Calling,
            peer_id: String::new(),
            peer_uid: 0.0,
            local_uid: 0.0,
            peer_name: String::new(),
            avatar: String::new(),
            call_type: 0.0,
            call_id: 0.0,
            sess_id: Value::Null,
            server: Value::Null,
            config: Value::Null,
            p2p: Value::Null,
            started_at: 0.0,
            deadline: None,
            muted: false,
            group: false,
            host_call: 0.0,
            group_id_str: String::new(),
            group_id: 0.0,
            title: String::new(),
            max_users: 8.0,
            invited: Vec::new(),
            invite_partners: None,
            members: Vec::new(),
            ping_ms: 9000.0,
            answered: false,
            ping_deadline: None,
            greeted: Vec::new(),
            members_key: String::new(),
            has_status_text: false,
            status_text: String::new(),
            camera_on: None,
            ring_deadline: None,
        }
    }
}

fn str_or(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or_default().to_string()
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Object(_)) | Some(Value::Array(_)) => true,
        _ => false,
    }
}

fn num_of(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
}

fn obj(v: Option<&Value>) -> Map<String, Value> {
    v.and_then(Value::as_object).cloned().unwrap_or_default()
}

/// Parses a stringified-JSON field (`params`, `extendData` arrive double-encoded).
pub(crate) fn parse_maybe_json_value(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => Value::Null,
    }
}

fn parse_maybe_json(v: Option<&Value>) -> Map<String, Value> {
    match v {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).ok().and_then(|p| p.as_object().cloned()).unwrap_or_default(),
        Some(Value::Object(o)) => o.clone(),
        _ => Map::new(),
    }
}

/// The engine. One call at a time, exactly like the original.
pub struct Engine<M: Media> {
    pub media: M,
    /// `update/init` payload (local id, avatar, …) — the fields 401 falls back
    /// to for `localUid`.
    local: Value,
    pub(crate) call: Option<Call>,
    /// Ended-call dedup cache: (callId, seen-at), TTL 60 s, ≤256 entries.
    ended: VecDeque<(f64, f64)>,
}

impl<M: Media> Engine<M> {
    pub fn new(media: M, local: Value) -> Self {
        Self { media, local, call: None, ended: VecDeque::new() }
    }

    pub fn in_call(&self) -> bool {
        self.call.is_some()
    }

    // ------------------------------------------------------------- renderer side

    /// `request/makeCall` — outgoing 1-1. Group calls are phase 3.
    pub fn make_call(&mut self, now: f64, data: &Value) -> Vec<Out> {
        if self.call.is_some() {
            return vec![];
        }
        let d = obj(Some(data));
        let call_type = num_of(d.get("type"));
        let group_info = d.get("groupInfo");
        if matches!(call_type, 5.0 | 6.0) || truthy(group_info) {
            return self.group_outgoing(now, data);
        }
        {
            let partner = d.get("partner").and_then(Value::as_array).and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
            let p = obj(Some(&partner));
            let call = Call {
                role: Role::Caller,
                state: State::Calling,
                peer_id: str_or(p.get("id")),
                peer_uid: 0.0,
                local_uid: 0.0,
                peer_name: str_or(p.get("name")),
                avatar: str_or(p.get("avatar")),
                call_type,
                call_id: self.new_call_id(now),
                sess_id: Value::Null,
                server: Value::Null,
                config: Value::Null,
                p2p: Value::Null,
                started_at: 0.0,
                deadline: None,
                muted: false,
                group: false,
                host_call: 0.0,
                group_id_str: String::new(),
                group_id: 0.0,
                title: String::new(),
                max_users: 8.0,
                invited: Vec::new(),
                invite_partners: None,
                members: Vec::new(),
                ping_ms: 9000.0,
                answered: false,
                ping_deadline: None,
                greeted: Vec::new(),
                members_key: String::new(),
                has_status_text: false,
                status_text: String::new(),
                camera_on: None,
                ring_deadline: None,
            };
            let peer_name = call.peer_name.clone();
            let avatar = call.avatar.clone();
            let call_id = call.call_id;
            let peer_id = call.peer_id.clone();
            let call_type = call.call_type;
            self.call = Some(call);
            let mut out = vec![
                Out::Update { command: "callState".into(), data: json!({"state": "incall"}) },
                Out::Response { command: "show".into(), data: json!({"error": 0, "extendData": "success"}) },
                Out::SendSignal { code: 401, data: json!({"callId": num_json(call_id), "calleeId": peer_id, "codec": CODEC_EMPTY, "type": num_json(call_type)}) },
                Out::Ui(UiEvent::Open {
                    title: if peer_name.is_empty() { "Zalo".into() } else { peer_name },
                    status_code: "calling".into(),
                    avatar,
                    video: call_type == 3.0,
                    self_avatar: self.self_avatar(),
                }),
            ];
            out.extend(self.set_deadline(now, CALL_SETUP_TIMEOUT_MS));
            out
        }
    }

    /// `request/endCall` and the UI hang-up button.
    pub fn hangup(&mut self, now: f64, why: &str) -> Vec<Out> {
        let Some(call) = &self.call else { return vec![] };
        if call.state == State::Ended {
            return vec![];
        }
        if call.group {
            return self.group_hangup(now, why);
        }
        if call.state == State::Incoming {
            return self.reject(now, why);
        }
        let (code, status) = if call.state == State::Connected {
            (409, 50)
        } else {
            (405, 103)
        };
        let mut out = vec![Out::SendSignal {
            code,
            data: if code == 409 {
                json!({"callId": num_json(call.call_id), "toId": call.peer_id})
            } else {
                json!({"callId": num_json(call.call_id), "callType": if call.call_type == 3.0 { 1 } else { 0 }, "status": 0, "toId": call.peer_id})
            },
        }];
        let close = if why == "no-server" { "serverUnreachable" } else { "" };
        out.extend(self.finish(now, status, close, true));
        out
    }

    /// UI accept of an incoming call.
    pub fn accept(&mut self, now: f64) -> Vec<Out> {
        if self.call.as_ref().is_some_and(|c| c.group && c.state == State::Incoming) {
            return self.group_answer(now);
        }
        let Some(call) = &mut self.call else { return vec![] };
        if call.role != Role::Callee || call.state != State::Incoming {
            return vec![];
        }
        call.state = State::Connected;
        call.started_at = now;
        let params = NegotiatedParams {
            muted: call.muted,
            speaker_off: false,
            servers: json!([call.server]),
            config: call.config.clone(),
            srtp_mode: call.config.get("srtpMode").cloned().unwrap_or(Value::Null),
            sess_id: call.sess_id.as_str().unwrap_or_default().to_string(),
            call_id: call.call_id,
            role: call.role,
            local_uid: call.local_uid,
            peer_uid: call.peer_uid,
            p2p: call.p2p.clone(),
            group: false,
            host: false,
            host_call: 0.0,
        };
        self.media.on_negotiated(&params);
        self.media.prepare(Role::Callee);
        vec![
            Out::Ui(UiEvent::Status("connecting".into())),
        ]
        // The 402 goes out when the relay registration completes (`media_prepared`).
    }

    /// Relay registration completed. `None` = no server answered.
    pub fn media_prepared(&mut self, now: f64, best: Option<InitReply>) -> Vec<Out> {
        if self.call.as_ref().is_some_and(|c| c.group) {
            return self.group_prepared(now, &best);
        }
        let Some(call) = &mut self.call else { return vec![] };
        let role = call.role;
        if role != Role::Callee {
            return self.outgoing_prepared(now, best);
        }
        if call.state != State::Connected && call.state != State::Incoming {
            return vec![];
        }
        let Some(call) = &self.call else { return vec![] };
        let server = &call.server;
        let extra = json!({"numServers": 0, "p2p": [], "sP2P": 1, "select2side": 1, "supportCallBusy": 0});
        let extend = extend_data(call, server, &None, &extra);
        let mut data = json!({
            "callId": num_json(call.call_id),
            "callerId": call.peer_id,
            "codec": CODEC_VOICE,
            "extendData": extend,
            "session": call.sess_id,
            "status": 0,
        });
        data["rtcpAddress"] = server.get("rtcpaddr").cloned().unwrap_or(Value::Null);
        data["rtpAddress"] = server.get("rtpaddr").cloned().unwrap_or(Value::Null);
        let mut out = vec![Out::SendSignal { code: 402, data }];
        if best.is_none() && self.media.send_udp() {
            out.push(Out::Ui(UiEvent::Status("serverUnreachable".into())));
            out.extend(self.hangup(now, "no-server"));
            return out;
        }
        let rtpaddr = str_or(server.get("rtpaddr"));
        self.media.start(Role::Callee, &rtpaddr);
        out.push(Out::Ui(UiEvent::Status("connected".into())));
        out
    }

    fn outgoing_prepared(&mut self, now: f64, best: Option<InitReply>) -> Vec<Out> {
        let Some(call) = &self.call else { return vec![] };
        if call.state != State::Calling {
            return vec![];
        }
        if best.is_none() && self.media.send_udp() {
            let mut out = vec![Out::Ui(UiEvent::Status("serverUnreachable".into()))];
            out.extend(self.hangup(now, "no-server"));
            return out;
        }
        let chosen = best
            .as_ref()
            .map(|b| b.server.clone())
            .unwrap_or_else(|| call.server.as_array().and_then(|a| a.first().cloned()).unwrap_or(Value::Null));
        let public_addr = best.as_ref().and_then(|b| b.public_addr.clone());
        let extend = extend_data(call, &chosen, &public_addr, &Value::Null);
        let mut data = json!({
            "callId": num_json(call.call_id),
            "calleeId": call.peer_id,
            "codec": CODEC_VOICE,
            "extendData": extend,
            "session": call.sess_id,
        });
        data["rtcpAddress"] = chosen.get("rtcpaddr").cloned().unwrap_or(Value::Null);
        data["rtpAddress"] = chosen.get("rtpaddr").cloned().unwrap_or(Value::Null);
        vec![Out::SendSignal { code: 416, data }]
    }

    /// UI decline of an incoming call.
    pub fn reject(&mut self, now: f64, why: &str) -> Vec<Out> {
        if self.call.as_ref().is_some_and(|c| c.group && c.state == State::Incoming) {
            return self.group_reject(now);
        }
        let Some(call) = &self.call else { return vec![] };
        if call.role != Role::Callee || call.state != State::Incoming {
            return vec![];
        }
        if why == "no-device" {
            return self.finish(now, 0, "", false);
        }
        let mut out = vec![Out::SendSignal {
            code: 402,
            data: json!({
                "callId": num_json(call.call_id),
                "callerId": call.peer_id,
                "codec": CODEC_EMPTY,
                "extendData": "{}\n",
                "rtcpAddress": "",
                "rtpAddress": "",
                "session": call.sess_id,
                "status": 3,
            }),
        }];
        out.extend(self.finish(now, 160, "", true));
        out
    }

    // ------------------------------------------------------------- server side

    /// A voip control frame (`act` + the control `data` object).
    pub fn handle_control(&mut self, now: f64, act: &str, data: &Value) -> Vec<Out> {
        let d = obj(data.get("data").filter(|v| v.is_object()));
        if act.starts_with("group_") {
            return self.on_group_control(now, act, &d);
        }
        let call_id = d
            .get("callId")
            .filter(|v| !v.is_null())
            .or_else(|| d.get("id").filter(|v| !v.is_null()))
            .or_else(|| data.get("callId").filter(|v| !v.is_null()))
            .or_else(|| data.get("id").filter(|v| !v.is_null()));
        let has_id = call_id.is_some();
        let other_call = self.call.as_ref().is_some_and(|c| {
            has_id && num_of(call_id) != c.call_id
        });
        match act {
            // The original passes the *inner* data object to incoming(); the
            // wrapper (act + data) is this function's own argument.
            "request" => self.incoming(now, &Value::Object(d)),
            "ring_ring" => {
                if let Some(call) = &mut self.call {
                    if call.state == State::Calling {
                        call.state = State::Ringing;
                        let mut out = vec![Out::Ui(UiEvent::Status("ringing".into()))];
                        out.extend(self.set_deadline(now, RING_TIMEOUT_MS));
                        return out;
                    }
                }
                vec![]
            }
            "answer" => {
                let mut out = vec![];
                if has_id {
                    out.extend(self.remember_ended(num_of(call_id), now));
                }
                if other_call {
                    return out;
                }
                let state = self.call.as_ref().map(|c| c.state);
                if state == Some(State::Incoming) {
                    out.extend(self.finish(now, 0, "", false));
                    return out;
                }
                let is_caller = self.call.as_ref().is_some_and(|c| c.role == Role::Caller);
                if !self.call.is_some() || !is_caller || state == Some(State::Connected) {
                    return out;
                }
                let params = parse_maybe_json(d.get("params"));
                let sv = if truthy(d.get("status")) { d.get("status") } else { params.get("status") };
                let sd = if truthy(sv) { num_of(sv) } else { 0.0 };
                if sd != 0.0 {
                    let close = if sd == 3.0 { "recipientDeclined" } else { "recipientUnreachable" };
                    out.extend(self.finish(now, 160, close, true));
                    return out;
                }
                out.extend(self.connect_caller(now, &str_or(params.get("rtpSerIp"))));
                out
            }
            "end_call" | "cancel" | "reject" | "busy" => {
                let mut out = vec![];
                if has_id {
                    out.extend(self.remember_ended(num_of(call_id), now));
                }
                if other_call {
                    return out;
                }
                let state = self.call.as_ref().map(|c| c.state);
                if state == Some(State::Incoming) {
                    out.extend(self.finish(now, 0, "", false));
                    return out;
                }
                out.extend(self.peer_ended(now));
                out
            }
            "answer_ack" => vec![],
            _ => vec![],
        }
    }

    /// A `recvSignal` reply to one of our signals.
    pub fn handle_recv_signal(&mut self, now: f64, command: &str, data: &Value) -> Vec<Out> {
        if command == "12433" {
            return self.group_request_answered(now, data);
        }
        if command == "12097" {
            // Group ping reply: error != 0 → finish trừ khi media còn chảy.
            let reply = parse_maybe_json_value(data.get("data"));
            let error = reply.get("error").and_then(Value::as_f64).unwrap_or(0.0);
            if self.call.as_ref().is_some_and(|c| c.group && c.state == State::Connected) && error != 0.0 {
                return self.group_ping_failed(now, error, Some(f64::INFINITY));
            }
            return vec![];
        }
        if command == "401" {
            let Some(call) = &mut self.call else { return vec![] };
            if call.state != State::Calling {
                return vec![];
            }
            let servers = data.get("servers").cloned().unwrap_or(Value::Null);
            let ok = truthy(data.get("status"));
            let empty = servers.as_array().map(|a| a.is_empty()).unwrap_or(true);
            if ok || !servers.is_array() || empty {
                let mut out = vec![Out::Ui(UiEvent::Status("callFailed".into()))];
                out.extend(self.finish(now, 103, "", true));
                return out;
            }
            call.sess_id = data.get("sessId").cloned().unwrap_or(Value::Null);
            call.local_uid = num_of(data.get("fromId"));
            if call.local_uid == 0.0 {
                call.local_uid = num_of(self.local.get("id"));
            }
            call.server = servers.clone();
            call.config = if truthy(data.get("zrtc_config")) { data.get("zrtc_config").cloned().unwrap() } else { Value::Null };
            let params = NegotiatedParams {
                muted: call.muted,
                speaker_off: false,
                servers: servers.clone(),
                config: call.config.clone(),
                srtp_mode: call.config.get("srtpMode").cloned().unwrap_or(Value::Null),
                sess_id: call.sess_id.as_str().unwrap_or_default().to_string(),
                call_id: call.call_id,
                role: call.role,
                local_uid: call.local_uid,
                peer_uid: num_of(data.get("toId")),
                p2p: Value::Null,
                group: false,
                host: false,
                host_call: 0.0,
            };
            let call_id = call.call_id;
            self.media.on_negotiated(&params);
            self.media.prepare(Role::Caller);
            let _ = call_id;
            // The chosen server (best reply or the first one) goes out via
            // `media_prepared` when the registration completes.
            return vec![];
        }
        if command == "409" || command == "405" {
            if let Some(call) = &self.call {
                if call.state != State::Ended {
                    if call.state == State::Incoming {
                        return self.finish(now, 0, "", false);
                    }
                    return self.peer_ended(now);
                }
            }
            return vec![];
        }
        // 402/407/416/408/406/12006/12064 are acks; everything else is logged upstream.
        vec![]
    }

    // ------------------------------------------------------------- media callbacks

    /// Peer audio arrived before the answer signal — the original's heuristic.
    pub fn early_audio(&mut self, now: f64, phase: &str) -> Vec<Out> {
        let Some(call) = &self.call else { return vec![] };
        if call.role != Role::Caller || call.state == State::Connected || call.state == State::Ended {
            return vec![];
        }
        if phase == "first" && call.state == State::Calling {
            if let Some(call) = &mut self.call {
                call.state = State::Ringing;
            }
            let mut out = vec![Out::Ui(UiEvent::Status("ringing".into()))];
            out.extend(self.set_deadline(now, RING_TIMEOUT_MS));
            return out;
        }
        if phase == "stream" {
            return self.connect_caller(now, "");
        }
        vec![]
    }

    /// 20 s without media — the engine's own watchdog fires via `tick`.
    pub fn media_timeout(&mut self, now: f64) -> Vec<Out> {
        self.hangup(now, "media-timeout")
    }

    // ------------------------------------------------------------- internals

    pub(crate) fn new_call_id(&self, now: f64) -> f64 {
        // Random in [1e9, 2^31), like the original. The seed mixes the clock
        // and a process-unique counter; the id only has to be unique per call.
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) as f64;
        let frac = (nanos / 1000.0 + now * 7.0) % 1.0;
        1e9 + frac * (2f64.powi(31) - 1e9)
    }

    pub(crate) fn self_avatar(&self) -> String {
        str_or(self.local.get("avatar"))
    }

    fn set_deadline(&mut self, now: f64, ms: f64) -> Vec<Out> {
        if let Some(call) = &mut self.call {
            call.deadline = Some((now + ms, if call.state == State::Calling { Deadline::Setup } else { Deadline::Ring }));
        }
        vec![]
    }

    /// Fires the call deadline when due; returns the events (usually none).
    pub fn tick(&mut self, now: f64) -> Vec<Out> {
        if self.call.as_ref().is_some_and(|c| c.group) {
            return self.group_tick(now);
        }
        let due = self.call.as_ref().and_then(|c| c.deadline).filter(|(at, _)| *at <= now);
        let Some((_, kind)) = due else { return vec![] };
        let state = self.call.as_ref().map(|c| c.state);
        if matches!(state, Some(State::Connected) | Some(State::Ended)) {
            return vec![];
        }
        let unreachable = matches!(kind, Deadline::Setup);
        let close = if unreachable { "recipientUnreachable" } else { "recipientNoAnswer" };
        let mut out = vec![Out::Ui(UiEvent::Status(close.into()))];
        if let Some(call) = &self.call {
            out.push(Out::SendSignal {
                code: 405,
                data: json!({
                    "callId": num_json(call.call_id),
                    "callType": if call.call_type == 3.0 { 1 } else { 0 },
                    "status": 0,
                    "toId": call.peer_id,
                }),
            });
        }
        out.extend(self.finish(now, 103, close, true));
        out
    }

    fn connect_caller(&mut self, now: f64, rtp_ser_ip: &str) -> Vec<Out> {
        let Some(call) = &mut self.call else { return vec![] };
        if call.role != Role::Caller || call.state == State::Connected || call.state == State::Ended {
            return vec![];
        }
        call.state = State::Connected;
        call.started_at = now;
        call.deadline = None;
        let call_id = call.call_id;
        let peer_id = call.peer_id.clone();
        self.media.start(Role::Caller, rtp_ser_ip);
        vec![
            Out::SendSignal { code: 408, data: json!({"callId": num_json(call_id), "calleeId": peer_id}) },
            Out::Ui(UiEvent::Status("connected".into())),
        ]
    }

    fn incoming(&mut self, now: f64, data: &Value) -> Vec<Out> {
        self.prune_ended(now);
        let d = obj(Some(data));
        let params = parse_maybe_json(d.get("params"));
        let ext = parse_maybe_json(params.get("extendData"));
        if self.is_ended(num_of(d.get("callId"))) {
            return vec![];
        }
        if self.call.is_some() {
            return vec![];
        }
        let server_addr = ext.get("serverAddr").and_then(Value::as_array).and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
        let addr = obj(Some(&server_addr));
        let call = Call {
            role: Role::Callee,
            state: State::Incoming,
            peer_id: str_or(d.get("uidN")),
            peer_uid: num_of(d.get("uidFrom")),
            local_uid: num_of(d.get("uidTo")),
            peer_name: str_or(params.get("Dname")),
            avatar: str_or(params.get("avatar")),
            call_type: if ext.get("callType").and_then(Value::as_f64) == Some(1.0) { 3.0 } else { 1.0 },
            call_id: num_of(d.get("callId")),
            sess_id: if truthy(d.get("session")) { d.get("session").cloned().unwrap() } else { params.get("sessId").cloned().unwrap_or(Value::Null) },
            server: json!({
                "rtpaddr": d.get("rtpAddress").cloned().unwrap_or(Value::Null),
                "rtcpaddr": d.get("rtcpAddress").cloned().unwrap_or(Value::Null),
                "rtpIPv6": str_or(addr.get("rtpIPv6")),
                "rtcpIPv6": str_or(addr.get("rtcpIPv6")),
            }),
            config: if truthy(params.get("zrtc_config")) { params.get("zrtc_config").cloned().unwrap() } else { Value::Null },
            p2p: ext.get("p2p").cloned().unwrap_or(Value::Null),
            started_at: 0.0,
            deadline: None,
            muted: false,
            group: false,
            host_call: 0.0,
            group_id_str: String::new(),
            group_id: 0.0,
            title: String::new(),
            max_users: 8.0,
            invited: Vec::new(),
            invite_partners: None,
            members: Vec::new(),
            ping_ms: 9000.0,
            answered: false,
            ping_deadline: None,
            greeted: Vec::new(),
            members_key: String::new(),
            has_status_text: false,
            status_text: String::new(),
            camera_on: None,
            ring_deadline: None,
        };
        let peer_name = call.peer_name.clone();
        let avatar = call.avatar.clone();
        let video = call.call_type == 3.0;
        let call_id = call.call_id;
        let caller_id = call.peer_id.clone();
        self.call = Some(call);
        vec![
            Out::Update { command: "callState".into(), data: json!({"state": "incall"}) },
            Out::Request { command: "getAliasName".into(), data: json!({"noisedId": caller_id}) },
            Out::SendSignal { code: 407, data: json!({"callId": num_json(call_id), "callerId": caller_id}) },
            Out::Ui(UiEvent::Incoming {
                title: if peer_name.is_empty() { "Zalo".into() } else { peer_name },
                status_code: if video { "incomingVideo".into() } else { "incomingVoice".into() },
                avatar,
                video,
                self_avatar: self.self_avatar(),
            }),
        ]
    }

    fn peer_ended(&mut self, now: f64) -> Vec<Out> {
        let Some(call) = &self.call else { return vec![] };
        if call.state == State::Ended {
            return vec![];
        }
        let connected = call.state == State::Connected;
        let mut out = vec![];
        if connected {
            out.push(Out::Request {
                command: "showNotification".into(),
                data: json!({"isGroup": false, "reason": "partner_endcall", "toUid": call.peer_id, "userId": call.peer_id}),
            });
        }
        out.extend(self.finish(now, if connected { 50 } else { 103 }, "", true));
        out
    }

    fn finish(&mut self, now: f64, status: i64, close_code: &str, emit_signals: bool) -> Vec<Out> {
        if self.call.as_ref().is_some_and(|c| c.group) {
            return self.group_finish(now, status, close_code, emit_signals);
        }
        let (call_id, peer_id, was_connected, has_session, started_at, role_caller) = {
            let Some(call) = &self.call else { return vec![] };
            if call.state == State::Ended {
                return vec![];
            }
            (
                call.call_id,
                call.peer_id.clone(),
                call.state == State::Connected,
                truthy(Some(&call.sess_id)),
                call.started_at,
                call.role.as_bool(),
            )
        };
        self.remember_ended(call_id, now);
        {
            // The call was not ended between the check and here: only this
            // thread drives the engine.
            let Some(call) = &mut self.call else { return vec![] };
            call.state = State::Ended;
            call.deadline = None;
        }
        let (rx, tx) = if has_session { self.media.stats() } else { (0, 0) };
        self.media.stop();
        let duration = if was_connected { ((now - started_at) / 1000.0).round() as i64 } else { 0 };
        let mut out = Vec::new();
        if emit_signals {
            out.push(Out::Update {
                command: "bubble".into(),
                data: json!({"duration": duration, "partnerId": peer_id, "role": role_caller}),
            });
            let stats = json!({
                "Codec": ["opus/16000/1", "20"],
                "NewZRTC": 1,
                "RxTotalPkt": rx,
                "SrtpMode": 1,
                "TxTotalPkt": tx,
                "sysInfo": "native-engine",
            });
            out.push(Out::SendSignal {
                code: 406,
                data: json!({
                    "callId": num_json(call_id),
                    "data": serde_json::to_string(&stats).unwrap_or_default(),
                    "duration": duration,
                    "partnerId": peer_id,
                    "role": role_caller,
                    "status": status,
                }),
            });
        }
        out.push(Out::Update { command: "callState".into(), data: json!({"state": "free"}) });
        let close = if !close_code.is_empty() {
            close_code.to_string()
        } else if was_connected {
            "endedWithDuration".to_string()
        } else {
            "ended".to_string()
        };
        out.push(Out::Ui(UiEvent::Close { code: close }));
        self.call = None;
        out
    }

    fn remember_ended(&mut self, id: f64, now: f64) -> Vec<Out> {
        if !id.is_finite() || id != id.trunc() || id.abs() > 9_007_199_254_740_991.0 || id <= 0.0 {
            return vec![];
        }
        self.prune_ended(now);
        self.ended.retain(|(e, _)| *e != id);
        self.ended.push_back((id, now));
        while self.ended.len() > MAX_ENDED_CALLS {
            self.ended.pop_front();
        }
        vec![]
    }

    fn prune_ended(&mut self, now: f64) {
        let cutoff = now - ENDED_CALL_TTL_MS;
        while self.ended.front().is_some_and(|(_, at)| *at <= cutoff) {
            self.ended.pop_front();
        }
    }

    fn is_ended(&self, id: f64) -> bool {
        self.ended.iter().any(|(e, _)| *e == id)
    }
}

/// The `extendData` "session description": a fixed field set plus the server
/// addresses, our P2P candidate and the per-flow extras.
fn extend_data(call: &Call, server: &Value, public_addr: &Option<String>, extra: &Value) -> String {
    let mut p2p = Vec::new();
    if let Some(addr) = public_addr {
        if let Some((ip, port)) = addr.split_once('|') {
            p2p.push(json!({"ip": ip, "port": port.parse::<i64>().unwrap_or(0), "type": 1}));
        }
    }
    let sa = json!({
        "rtcp": server.get("rtcpaddr").cloned().unwrap_or(Value::Null),
        "rtcpIPv6": server.get("rtcpIPv6").cloned().unwrap_or(Value::Null),
        "rtp": server.get("rtpaddr").cloned().unwrap_or(Value::Null),
        "rtpIPv6": server.get("rtpIPv6").cloned().unwrap_or(Value::Null),
        "tpType": 0,
    });
    let mut o = json!({
        "callType": if call.call_type == 3.0 { 1 } else { 0 },
        "fecTP": 0,
        "gccAudio": 1,
        "gccEarlyCall": 0,
        "gccMode": 1,
        "gccSVLR": 1,
        "maxFT": 60,
        "newZrtc": 1,
        "p2p": p2p,
        "packetMode": 2,
        "platform": 2,
        "sP2P": 0,
        "serverAddr": [sa],
        "spTcp": 1,
        "srtcp": 0,
        "srtpMode": 1,
        "supportCallBusy": 1,
        "supportHevcDecode": 0,
        "tpType": 0,
        "video": {"codec": [{"name": "h264", "payload": 97}]},
    });
    if let (Some(base), Some(extra)) = (o.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    serde_json::to_string(&o).unwrap_or_default()
}
