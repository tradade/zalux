//! Group call state machine — the group paths of the Zalos client's
//! `engine_core.cpp` (BSD-3-Clause), as an extension block of [`Engine`].
//!
//! Signalling (all relayed by the renderer): `12433` request → server answer
//! carries session/servers/`hostCall` → `12434` join · `12436` answer
//! (status 0 join / 3 decline) · `12437` cancel · `12438` end · `12439`
//! ring-ring · `12097` ping (every `interval`) · `12098` broadcast state ·
//! `12446` finish log. Controls: `group_request/answer/ring_ring/broadcast/
//! end_call/cancel`. The media relay roster (`RoomUpdated`, cmd 50) is the
//! source of truth for who is actually in the room.

use super::engine::{Engine, Media, Member, MemberInfo, MemberKey, NegotiatedParams, Out, Role, State, UiEvent};
use serde_json::{json, Map, Value};

const GROUP_RING_TIMEOUT_MS: f64 = 60_000.0;
const GROUP_DEFAULT_PING_MS: f64 = 10_000.0;
const GROUP_MEDIA_IDLE_MS: f64 = 10_000.0;

fn str_or(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or_default().to_string()
}

fn num_of(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
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

fn parse_maybe_json(v: Option<&Value>) -> Map<String, Value> {
    match v {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).ok().and_then(|p| p.as_object().cloned()).unwrap_or_default(),
        Some(Value::Object(o)) => o.clone(),
        _ => Map::new(),
    }
}

impl<M: Media> Engine<M> {
    // ------------------------------------------------------------- outgoing

    pub(crate) fn group_outgoing(&mut self, now: f64, data: &Value) -> Vec<Out> {
        let d = data.as_object().cloned().unwrap_or_default();
        let g = d.get("groupInfo").and_then(Value::as_object).cloned().unwrap_or_default();
        let partners: Vec<Value> = match d.get("partner") {
            Some(Value::Array(a)) => a.clone(),
            Some(v) if !v.is_null() => vec![v.clone()],
            _ => vec![],
        };
        let mut ids: Vec<String> = Vec::new();
        let mut invite: Vec<(String, String, String)> = Vec::new();
        let mut members: Vec<(MemberKey, Member)> = Vec::new();
        for (i, p) in partners.iter().enumerate() {
            let po = p.as_object().cloned().unwrap_or_default();
            let id = str_or(po.get("id"));
            if !id.is_empty() {
                ids.push(id.clone());
            }
            let key = if truthy(po.get("id")) { MemberKey::Str(id.clone()) } else { MemberKey::Str((i + 1).to_string()) };
            let name = str_or(po.get("name"));
            let avatar = str_or(po.get("avatar"));
            members.push((
                key,
                Member { name: name.clone(), avatar: avatar.clone(), state: "calling".into(), muted: false, cam_off: true },
            ));
            invite.push((name, avatar, id));
        }
        let mu = num_of(g.get("maxUsers"));
        let call_id = self.new_call_id(now);
        let title = {
            let n = str_or(g.get("name"));
            let n = if n.is_empty() { str_or(g.get("groupName")) } else { n };
            if n.is_empty() { "Cuộc gọi nhóm".to_string() } else { n }
        };
        let avatar = str_or(g.get("avatar"));
        let group_id_str = str_or(g.get("id"));
        let call = super::engine::Call {
            group: true,
            role: Role::Caller,
            state: State::Calling,
            call_id,
            group_id_str: group_id_str.clone(),
            call_type: if num_of(d.get("type")) == 6.0 { 1.0 } else { 0.0 },
            title: title.clone(),
            avatar: avatar.clone(),
            max_users: if mu != 0.0 { mu } else { 8.0 },
            invited: ids.clone(),
            invite_partners: Some(invite),
            members,
            ping_ms: 9000.0,
            ..super::engine::Call::voice_template()
        };
        self.call = Some(call);
        let mut out = vec![
            Out::Update { command: "callState".into(), data: json!({"state": "incall"}) },
            Out::Response { command: "show".into(), data: json!({"error": 0, "extendData": "success"}) },
            Out::SendSignal {
                code: 12433,
                data: json!({
                    "callId": super::engine::num_json(call_id),
                    "data": {
                        "extraData": "",
                        "groupAvatar": avatar,
                        "groupId": group_id_str,
                        "groupName": title,
                        "maxUsers": self.call.as_ref().unwrap().max_users,
                        "noiseId": ids,
                    },
                    "groupId": group_id_str,
                    "partners": ids,
                    "typeRequest": 1,
                }),
            },
            Out::Ui(UiEvent::Open {
                title,
                status_code: "groupCalling".into(),
                avatar: self.call.as_ref().unwrap().avatar.clone(),
                video: true,
                self_avatar: self.self_avatar(),
            }),
        ];
        // Nobody answered in a minute → cancel.
        if let Some(call) = &mut self.call {
            call.ring_deadline = Some((now + GROUP_RING_TIMEOUT_MS, GROUP_RING_TIMEOUT_MS));
        }
        out.extend(self.group_status(now));
        out
    }

    /// The server answered our `12433`: session, servers and the host id.
    pub(crate) fn group_request_answered(&mut self, now: f64, data: &Value) -> Vec<Out> {
        let Some(call) = &mut self.call else { return vec![] };
        if !(call.group && call.role == Role::Caller && call.state == State::Calling) {
            return vec![];
        }
        let mut p = parse_maybe_json(data.get("data"));
        for (k, v) in parse_maybe_json(data.get("params")) {
            p.insert(k, v);
        }
        let setting = parse_maybe_json(p.get("callSetting"));
        let status = if p.contains_key("status") { num_of(p.get("status")) } else { num_of(data.get("status")) };
        call.host_call = if truthy(data.get("hostCall")) { num_of(data.get("hostCall")) } else { num_of(p.get("hostCall")) };
        call.local_uid = call.host_call;
        if truthy(p.get("callId")) {
            call.call_id = num_of(p.get("callId"));
        }
        if truthy(p.get("groupId")) && num_of(p.get("groupId")) < 0x7fff_ffff as f64 {
            call.group_id = num_of(p.get("groupId"));
        }
        if truthy(p.get("interval")) {
            let iv = num_of(p.get("interval"));
            call.ping_ms = if iv < 1000.0 { iv * 1000.0 } else { iv };
        }
        call.sess_id = Value::String({
            let s = str_or(setting.get("session"));
            if s.is_empty() { str_or(p.get("session")) } else { s }
        });
        call.server = setting.get("servers").cloned().unwrap_or(Value::Array(vec![]));
        call.config = Value::Object(parse_maybe_json(setting.get("zrtcConfig")));
        if status != 0.0 || str_or(Some(&call.sess_id)).is_empty() || call.server.as_array().map(|a| a.is_empty()).unwrap_or(true) || call.local_uid == 0.0 {
            call.state = State::Failed;
            let msg = str_or(p.get("msg"));
            let mut out = vec![];
            if !msg.is_empty() {
                out.push(Out::Ui(UiEvent::Status(format!("groupCallFailedReason:{msg}"))));
            } else {
                out.push(Out::Ui(UiEvent::Status("groupCallFailed".into())));
            }
            // The finish fires 3 s later via the deadline machinery.
            call.ring_deadline = Some((now + 3000.0, 3000.0));
            return out;
        }
        call.state = State::Connecting;
        let params = NegotiatedParams {
            muted: call.muted,
            speaker_off: false,
            servers: call.server.clone(),
            config: call.config.clone(),
            srtp_mode: call.config.get("srtpMode").cloned().unwrap_or(Value::Null),
            sess_id: str_or(Some(&call.sess_id)),
            call_id: call.call_id,
            role: call.role,
            local_uid: call.local_uid,
            peer_uid: 0.0,
            p2p: Value::Null,
            group: true,
            host: true,
            host_call: call.host_call,
        };
        self.media.on_negotiated(&params);
        self.media.prepare(Role::Caller);
        vec![]
    }

    // ------------------------------------------------------------- incoming

    pub(crate) fn group_incoming(&mut self, now: f64, d: &Value) -> Vec<Out> {
        if self.call.is_some() {
            return vec![];
        }
        let dobj = d.as_object().cloned().unwrap_or_default();
        let setting = parse_maybe_json(dobj.get("callSetting"));
        let local_uid = num_of(dobj.get("receiverId"));
        let mut members: Vec<(MemberKey, Member)> = Vec::new();
        if let Some(list) = dobj.get("partnerInfo").and_then(Value::as_array) {
            for pv in list {
                let p = pv.as_object().cloned().unwrap_or_default();
                let uid = num_of(p.get("userId"));
                if uid == 0.0 || uid == local_uid {
                    continue;
                }
                members.push((
                    MemberKey::of_uid(uid),
                    Member {
                        name: str_or(p.get("name")),
                        avatar: str_or(p.get("avatar")),
                        state: if num_of(p.get("callState")) == 3.0 { "incall".into() } else { "ringing".into() },
                        muted: num_of(p.get("audioState")) == 1.0,
                        cam_off: num_of(p.get("videoState")) == 1.0,
                    },
                ));
            }
        }
        let interval = num_of(dobj.get("interval"));
        let call = super::engine::Call {
            group: true,
            role: Role::Callee,
            state: State::Incoming,
            call_id: num_of(dobj.get("id")),
            host_call: num_of(dobj.get("hostCall")),
            group_id: num_of(dobj.get("groupId")),
            local_uid,
            call_type: if num_of(dobj.get("callType")) == 1.0 { 1.0 } else { 0.0 },
            sess_id: {
                let s = str_or(setting.get("session"));
                if s.is_empty() { Value::String(str_or(dobj.get("session"))) } else { Value::String(s) }
            },
            server: setting.get("servers").cloned().unwrap_or(Value::Array(vec![])),
            config: Value::Object(parse_maybe_json(setting.get("zrtcConfig"))),
            title: {
                let t = str_or(dobj.get("groupName"));
                if t.is_empty() { "Cuộc gọi nhóm".to_string() } else { t }
            },
            avatar: str_or(dobj.get("groupAvatar")),
            ping_ms: if interval != 0.0 { if interval < 1000.0 { interval * 1000.0 } else { interval } } else { GROUP_DEFAULT_PING_MS },
            members,
            ..super::engine::Call::voice_template()
        };
        let title = call.title.clone();
        let avatar = call.avatar.clone();
        let host_call = call.host_call;
        let call_id = call.call_id;
        let sess_ok = !str_or(Some(&call.sess_id)).is_empty();
        let servers_ok = call.server.as_array().map(|a| !a.is_empty()).unwrap_or(false);
        let members_for_ui = call.members.clone();
        if !sess_ok || !servers_ok || local_uid == 0.0 {
            // Incomplete group_request: not answerable — the renderer still
            // shows its own notice.
            return vec![];
        }
        self.call = Some(call);
        let mut out = vec![
            Out::Update { command: "callState".into(), data: json!({"state": "incall"}) },
            Out::SendSignal {
                code: 12439,
                data: json!({
                    "callId": super::engine::num_json(call_id),
                    "data": {"callType": 1, "extraData": "", "status": 1},
                    "hostCall": super::engine::num_json(host_call),
                    "session": self.call.as_ref().unwrap().sess_id,
                    "status": 1,
                }),
            },
            Out::Ui(UiEvent::Incoming {
                title,
                status_code: "incomingGroupInvite".into(),
                avatar,
                video: true,
                self_avatar: self.self_avatar(),
            }),
        ];
        let _ = members_for_ui;
        out
    }

    pub(crate) fn group_answer(&mut self, now: f64) -> Vec<Out> {
        let Some(call) = &mut self.call else { return vec![] };
        if !(call.group && call.state == State::Incoming) {
            return vec![];
        }
        call.state = State::Connecting;
        let params = NegotiatedParams {
            muted: call.muted,
            speaker_off: false,
            servers: call.server.clone(),
            config: call.config.clone(),
            srtp_mode: call.config.get("srtpMode").cloned().unwrap_or(Value::Null),
            sess_id: str_or(Some(&call.sess_id)),
            call_id: call.call_id,
            role: call.role,
            local_uid: call.local_uid,
            peer_uid: 0.0,
            p2p: Value::Null,
            group: true,
            host: false,
            host_call: call.host_call,
        };
        self.media.on_negotiated(&params);
        self.media.prepare(Role::Callee);
        vec![Out::Ui(UiEvent::Status("connecting".into()))]
    }

    pub(crate) fn group_reject(&mut self, now: f64) -> Vec<Out> {
        let Some(call) = &self.call else { return vec![] };
        if !(call.group && call.state == State::Incoming) {
            return vec![];
        }
        let mut out = self.group_send_answer(3);
        out.extend(self.group_finish(now, 0, "groupDeclined", false));
        out
    }

    fn group_send_answer(&self, status: i64) -> Vec<Out> {
        let Some(call) = &self.call else { return vec![] };
        vec![Out::SendSignal {
            code: 12436,
            data: json!({
                "callId": super::engine::num_json(call.call_id),
                "data": {"callType": call.call_type, "codec": "", "extendData": "", "groupId": super::engine::num_json(call.group_id), "status": status},
                "hostCall": super::engine::num_json(call.host_call),
                "session": call.sess_id,
                "status": status,
            }),
        }]
    }

    /// Relay registration done for a group call: join (12434) + start pinging.
    pub(crate) fn group_prepared(&mut self, now: f64, best: &Option<super::engine::InitReply>) -> Vec<Out> {
        let (call_id, call_type, host_call, max_users, group_id, group_id_str, avatar, title, invited, sess_id, is_caller, first_server) = {
            let Some(call) = &self.call else { return vec![] };
            if !call.group || call.state != State::Connecting {
                return vec![];
            }
            (
                call.call_id,
                call.call_type,
                call.host_call,
                call.max_users,
                super::engine::num_json(call.group_id),
                call.group_id_str.clone(),
                call.avatar.clone(),
                call.title.clone(),
                call.invited.clone(),
                call.sess_id.clone(),
                call.role == Role::Caller,
                call.server.as_array().and_then(|a| a.first().cloned()).unwrap_or(Value::Null),
            )
        };
        if best.is_none() && self.media.send_udp() {
            if let Some(call) = &mut self.call {
                call.state = State::Failed;
                call.ring_deadline = Some((now + 3000.0, 3000.0));
            }
            let mut out = vec![Out::Ui(UiEvent::Status("groupJoinRejected".into()))];
            if !is_caller {
                out.extend(self.group_send_answer(3));
            }
            return out;
        }
        let chosen = best
            .as_ref()
            .map(|b| b.server.clone())
            .unwrap_or(first_server);
        let pick = |a: &str| chosen.get(a).and_then(Value::as_str).unwrap_or_default().to_string();
        // Role thật của call (callee join bằng InitCallCallee).
        let join_role = self.call.as_ref().map(|c| c.role).unwrap_or(Role::Callee);
        self.media.start(join_role, "");
        if let Some(call) = &mut self.call {
            call.state = State::Connected;
            call.started_at = now;
            call.deadline = None;
            if !is_caller {
                call.answered = true;
            }
        }
        let mut out = Vec::new();
        if is_caller {
            // Host announces the room to the invited members.
            out.push(Out::SendSignal {
                code: 12434,
                data: json!({
                    "callId": super::engine::num_json(call_id),
                    "callType": call_type,
                    "data": {
                        "codec": "",
                        "data": {
                            "groupAvatar": avatar,
                            "groupName": title,
                            "hostCall": super::engine::num_json(host_call),
                            "maxUsers": max_users,
                            "noiseId": invited,
                        },
                        "extendData": "",
                        "rtcpAddress": pick("rtcpaddr"),
                        "rtcpAddressIPv6": pick("rtcpaddrIPv6"),
                        "rtpAddress": pick("rtpaddr"),
                        "rtpAddressIPv6": pick("rtpaddrIPv6"),
                    },
                    "groupId": group_id_str,
                    "partners": invited.clone(),
                    "session": sess_id,
                }),
            });
        } else {
            out.extend(self.group_send_answer(0));
        }
        out.extend(self.group_broadcast());
        if let Some(call) = &mut self.call {
            call.ping_deadline = Some(now + call.ping_ms);
        }
        out.extend(self.group_status(now));
        out
    }

    // ------------------------------------------------------------- group state

    pub(crate) fn group_state_json(&self) -> Value {
        let Some(call) = &self.call else { return json!({}) };
        json!({
            "audioState": if call.muted { 1 } else { 0 },
            "callId": super::engine::num_json(call.call_id),
            "callState": 3,
            "hostCall": super::engine::num_json(call.host_call),
            "userId": super::engine::num_json(call.local_uid),
            "videoState": if call.camera_on.unwrap_or(false) { 0 } else { 1 },
        })
    }

    pub(crate) fn group_broadcast(&mut self) -> Vec<Out> {
        if self.call.as_ref().is_some_and(|c| c.group && c.state == State::Connected) {
            return vec![Out::SendSignal { code: 12098, data: self.group_state_json() }];
        }
        vec![]
    }

    pub(crate) fn group_ping(&mut self) -> Vec<Out> {
        if self.call.as_ref().is_some_and(|c| c.group && c.state == State::Connected) {
            return vec![Out::SendSignal { code: 12097, data: self.group_state_json() }];
        }
        vec![]
    }

    fn set_member(&mut self, key: MemberKey, next: Member) {
        if let Some(call) = &mut self.call {
            if let Some((k, m)) = call.members.iter_mut().find(|(k, _)| *k == key) {
                *m = next;
            } else {
                call.members.push((key, next));
            }
        }
    }

    pub(crate) fn group_member(&mut self, now: f64, uid: f64, info: MemberInfo) -> Vec<Out> {
        if !self.call.as_ref().is_some_and(|c| c.group && uid != c.local_uid) {
            return vec![];
        }
        let key = MemberKey::of_uid(uid);
        // An invite keyed by string may now have its real uid.
        let existing: Option<Member> = {
            let call = self.call.as_mut().unwrap();
            match call.members.iter_mut().find(|(k, _)| *k == key) {
                Some((_, m)) => Some(m.clone()),
                None => {
                    let skey = MemberKey::of_str(super::engine::fmt_num(uid));
                    call.members.iter().position(|(k, _)| *k == skey).map(|pos| call.members.remove(pos).1)
                }
            }
        };
        let mut next = existing.clone().unwrap_or_default();
        if existing.is_none() {
            next.name = String::new();
            next.avatar = String::new();
            next.state = String::new();
        }
        if let Some(s) = &info.state {
            next.state = s.clone();
        }
        if let Some(m) = info.muted {
            next.muted = m;
        }
        if let Some(c) = info.cam_off {
            next.cam_off = c;
        }
        if next.name.is_empty() {
            if let Some(call) = &self.call {
                if let Some(invite) = &call.invite_partners {
                    if invite.len() == 1 {
                        next.name = invite[0].0.clone();
                        next.avatar = invite[0].1.clone();
                    }
                }
            }
        }
        self.set_member(key, next);
        let mut out = vec![];
        if info.state.as_deref() == Some("incall") {
            if let Some(call) = &mut self.call {
                call.answered = true;
                call.ring_deadline = None;
            }
        }
        out.extend(self.group_status(now));
        out
    }

    /// The media relay's `RoomUpdated` roster: who is really in the room.
    pub(crate) fn group_roster(&mut self, now: f64, uids: &[u32]) -> Vec<Out> {
        let local_uid = self.call.as_ref().map(|c| c.local_uid).unwrap_or(0.0);
        let mut in_room: Vec<f64> = Vec::new();
        for u in uids {
            let u = f64::from(*u);
            if !in_room.contains(&u) {
                in_room.push(u);
            }
        }
        let mut out = vec![];
        for uid in in_room.clone() {
            if uid == local_uid {
                continue;
            }
            let in_call = self.call.as_ref().is_some_and(|c| {
                c.members.iter().any(|(k, m)| *k == MemberKey::of_uid(uid) && m.state == "incall")
            });
            if !in_call {
                // A fresh joiner gets a greet (our state re-broadcast).
                let greeted = self.call.as_ref().is_some_and(|c| c.greeted.contains(&uid));
                if !greeted {
                    if let Some(call) = &mut self.call {
                        call.greeted.push(uid);
                    }
                    out.extend(self.group_broadcast());
                }
            }
            out.extend(self.group_member(now, uid, MemberInfo { state: Some("incall".into()), muted: None, cam_off: None }));
        }
        if let Some(call) = &mut self.call {
            for (k, m) in call.members.iter_mut() {
                if let MemberKey::Uid(u) = k {
                    if m.state == "incall" && !in_room.contains(u) {
                        m.state = "left".into();
                    }
                }
            }
        }
        out.extend(self.group_status(now));
        out.extend(self.group_check_empty(now, "roster"));
        out
    }

    pub(crate) fn group_member_left(&mut self, now: f64, uid: f64, why: &str) -> Vec<Out> {
        let Some(call) = &mut self.call else { return vec![] };
        if !call.group || uid == call.local_uid {
            return vec![];
        }
        let key = MemberKey::of_uid(uid);
        let Some(entry) = call.members.iter_mut().find(|(k, _)| *k == key) else { return vec![] };
        if entry.1.state == "left" {
            return vec![];
        }
        entry.1.state = "left".into();
        let mut out = vec![];
        out.extend(self.group_status(now));
        out.extend(self.group_check_empty(now, why));
        out
    }

    fn group_check_empty(&mut self, now: f64, why: &str) -> Vec<Out> {
        let Some(call) = &self.call else { return vec![] };
        if !call.group || call.state != State::Connected {
            return vec![];
        }
        let anyone = call.members.iter().any(|(_, m)| matches!(m.state.as_str(), "incall" | "ringing" | "calling"));
        if anyone {
            return vec![];
        }
        let mut out = vec![];
        out.extend(self.group_hangup(now, "alone"));
        let _ = why;
        out
    }

    pub(crate) fn group_status(&mut self, now: f64) -> Vec<Out> {
        let Some(call) = &self.call else { return vec![] };
        if !(call.group && matches!(call.state, State::Connected | State::Calling)) {
            return vec![];
        }
        let mut names: Vec<Value> = Vec::new();
        let mut has_ringing = false;
        for (_, m) in &call.members {
            if m.state == "ringing" {
                has_ringing = true;
            }
            if m.state == "incall" && !m.name.is_empty() {
                names.push(json!(m.name));
            }
        }
        let waiting = if call.role == Role::Caller && !call.answered {
            if has_ringing { "groupRinging" } else { "groupCalling" }
        } else {
            "groupWaitingOthers"
        };
        let code = if !names.is_empty() { "groupParticipants" } else { waiting };
        let params = if !names.is_empty() {
            json!({"count": names.len() + 1, "names": names})
        } else {
            json!({})
        };
        let key = format!("{}{}", code, params.to_string());
        let changed = !call.has_status_text || call.status_text != key;
        if !changed {
            return vec![];
        }
        if let Some(call) = &mut self.call {
            call.has_status_text = true;
            call.status_text = key;
        }
        vec![Out::Ui(UiEvent::Status(format!("{code}:{params}")))]
    }

    // ------------------------------------------------------------- end paths

    pub(crate) fn group_ended(&mut self, now: f64, why: &str) -> Vec<Out> {
        let Some(call) = &self.call else { return vec![] };
        if call.state == State::Ended {
            return vec![];
        }
        let status = if call.state == State::Connected { 50 } else { 0 };
        let close = if why == "kicked" { "groupKicked" } else { "groupEnded" };
        self.group_finish(now, status, close, true)
    }

    pub(crate) fn group_hangup(&mut self, now: f64, why: &str) -> Vec<Out> {
        let Some(call) = &self.call else { return vec![] };
        if call.state == State::Ended {
            return vec![];
        }
        if call.state == State::Incoming {
            return self.group_reject(now);
        }
        if call.role == Role::Caller && !call.answered {
            return self.group_cancel(now, why);
        }
        let status = if call.state == State::Connected { 50 } else { 0 };
        self.group_finish(now, status, "", true)
    }

    fn group_cancel(&mut self, now: f64, why: &str) -> Vec<Out> {
        let Some(call) = &self.call else { return vec![] };
        let mut out = vec![];
        if call.host_call != 0.0 {
            out.push(Out::SendSignal {
                code: 12437,
                data: json!({
                    "callId": super::engine::num_json(call.call_id),
                    "data": {"callType": call.call_type, "duration": 0, "extraData": "", "groupId": super::engine::num_json(call.group_id)},
                    "hostCall": super::engine::num_json(call.host_call),
                }),
            });
        }
        let close = if why == "no-answer" { "groupNoAnswer" } else { "groupCancelled" };
        out.extend(self.group_finish(now, 0, close, false));
        out
    }

    pub(crate) fn group_finish(&mut self, now: f64, status: i64, close_code: &str, _emit_signals: bool) -> Vec<Out> {
        let (call_id, host_call, call_type, group_id, was_connected, started_at, sess_truthy) = {
            let Some(call) = &self.call else { return vec![] };
            if call.state == State::Ended {
                return vec![];
            }
            (
                call.call_id,
                call.host_call,
                call.call_type,
                call.group_id,
                call.state == State::Connected,
                call.started_at,
                truthy(Some(&call.sess_id)),
            )
        };
        if let Some(call) = &mut self.call {
            call.state = State::Ended;
            call.deadline = None;
            call.ping_deadline = None;
            call.ring_deadline = None;
        }
        let (rx, tx) = if was_connected && sess_truthy { self.media.stats() } else { (0, 0) };
        self.media.stop();
        let duration = if was_connected { ((now - started_at) / 1000.0).round() as i64 } else { 0 };
        let mut out = Vec::new();
        if _emit_signals && was_connected {
            out.push(Out::SendSignal {
                code: 12438,
                data: json!({
                    "callId": super::engine::num_json(call_id),
                    "data": {"callType": call_type, "duration": duration, "extraData": "", "groupId": group_id, "status": 0},
                    "hostCall": super::engine::num_json(host_call),
                }),
            });
            let stats = json!({
                "Codec": ["opus/16000/1", "20"],
                "RxTotalPkt": rx,
                "TxTotalPkt": tx,
                "sysInfo": "native-engine",
            });
            out.push(Out::SendSignal {
                code: 12446,
                data: json!({
                    "callId": super::engine::num_json(call_id),
                    "callType": call_type,
                    "data": serde_json::to_string(&stats).unwrap_or_default(),
                    "duration": duration,
                    "hostCall": super::engine::num_json(host_call),
                    "joinTime": super::engine::num_json(started_at),
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

    /// The group ping failed: tolerate while media flowed, else finish.
    pub(crate) fn group_ping_failed(&mut self, now: f64, error: f64, last_rx_ms_ago: Option<f64>) -> Vec<Out> {
        if error == -3.0 && self.call.as_ref().is_some_and(|c| c.role == Role::Caller && !c.answered) {
            return vec![];
        }
        if error != 0.0 {
            let idle_ok = last_rx_ms_ago.is_some_and(|idle| idle < GROUP_MEDIA_IDLE_MS);
            if idle_ok {
                return vec![]; // media still flowing; stay in the call
            }
            return self.group_finish(now, 50, "groupEnded", true);
        }
        vec![]
    }

    /// Control acts with the `group_` prefix.
    pub(crate) fn on_group_control(&mut self, now: f64, act: &str, d: &Map<String, Value>) -> Vec<Out> {
        let id = d.get("id").or_else(|| d.get("callId")).cloned();
        if let Some(call) = &self.call {
            if let Some(id) = &id {
                if num_of(Some(id)) != call.call_id && !id.is_null() {
                    return vec![];
                }
            }
        }
        let who = if truthy(d.get("userId")) { num_of(d.get("userId")) } else { num_of(d.get("fromId")) };
        let local_uid = self.call.as_ref().map(|c| c.local_uid).unwrap_or(0.0);
        let host_call = self.call.as_ref().map(|c| c.host_call).unwrap_or(0.0);
        match act {
            "group_request" => self.group_incoming(now, &Value::Object(d.clone())),
            "group_broadcast" => {
                if who != 0.0 && who != local_uid {
                    let info = MemberInfo {
                        state: Some(if num_of(d.get("callState")) == 3.0 { "incall".into() } else { "ringing".into() }),
                        muted: Some(num_of(d.get("audioState")) == 1.0),
                        cam_off: Some(num_of(d.get("videoState")) == 1.0),
                    };
                    return self.group_member(now, who, info);
                }
                vec![]
            }
            "group_answer" => {
                let state = self.call.as_ref().map(|c| c.state);
                if state == Some(State::Incoming) && (who == 0.0 || who == local_uid) {
                    // Answered on another device.
                    return self.group_finish(now, 0, "groupEnded", false);
                }
                if who != 0.0 && who != local_uid {
                    let mut out = self.group_member(now, who, MemberInfo { state: Some("incall".into()), muted: None, cam_off: None });
                    // Greet: re-broadcast our state.
                    out.extend(self.group_broadcast());
                    return out;
                }
                vec![]
            }
            "group_ring_ring" => {
                if who != 0.0 && who != local_uid {
                    let in_call = self.call.as_ref().map(|c| {
                        c.members.iter().any(|(k, m)| *k == MemberKey::of_uid(who) && m.state == "incall")
                    }).unwrap_or(false);
                    if !in_call {
                        return self.group_member(now, who, MemberInfo { state: Some("ringing".into()), muted: None, cam_off: None });
                    }
                }
                vec![]
            }
            "group_end_call" | "group_cancel" => {
                let state = self.call.as_ref().map(|c| c.state);
                if state == Some(State::Incoming) && (who == 0.0 || who == host_call || who == local_uid) {
                    return self.group_finish(now, 0, "groupEnded", false);
                }
                if who == local_uid {
                    let status = if state == Some(State::Connected) { 50 } else { 0 };
                    return self.group_finish(now, status, "groupEnded", false);
                }
                if who != 0.0 {
                    return self.group_member_left(now, who, act);
                }
                vec![]
            }
            _ => vec![],
        }
    }
}

impl<M: Media> Engine<M> {
    /// Group tick: repeating ping, no-answer cancel, deferred finishes.
    pub(crate) fn group_tick(&mut self, now: f64) -> Vec<Out> {
        let mut out = vec![];
        // Repeating ping.
        let ping_due = self.call.as_ref().and_then(|c| c.ping_deadline).filter(|at| *at <= now);
        if ping_due.is_some() {
            let interval = self.call.as_ref().map(|c| c.ping_ms).unwrap_or(GROUP_DEFAULT_PING_MS);
            if let Some(call) = &mut self.call {
                call.ping_deadline = Some(now + interval);
            }
            out.extend(self.group_ping());
        }
        // One-shot deadlines: group no-answer (60 s) / deferred finish (3 s).
        let ring_due = self.call.as_ref().and_then(|c| c.ring_deadline).filter(|(at, _)| *at <= now);
        if ring_due.is_some() {
            let (at, ms) = ring_due.unwrap();
            if let Some(call) = &mut self.call {
                call.ring_deadline = None;
            }
            let state = self.call.as_ref().map(|c| (c.state, c.role, c.answered));
            match state {
                // Deferred finish (3 s) sau Failed/JoinRejected.
                Some((State::Failed, _, _)) | Some((State::Connecting, _, _)) if ms == 3000.0 => {
                    let failed = self.call.as_ref().map(|c| c.state == State::Failed).unwrap_or(false);
                    let code = if failed { "groupCallFailed" } else { "groupJoinRejected" };
                    out.extend(self.group_finish(now, 0, code, false));
                }
                // No-answer (60 s): chưa ai join → cancel dù đã vào phòng.
                Some((_, Role::Caller, false)) if ms == GROUP_RING_TIMEOUT_MS => {
                    out.extend(self.group_cancel(now, "no-answer"));
                }
                _ => {}
            }
            let _ = at;
        }
        out
    }
}
