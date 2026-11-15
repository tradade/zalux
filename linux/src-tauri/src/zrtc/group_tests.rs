//! Group call state machine tests — flows against a fake media backend, the
//! signal shapes mirror `engine_core.cpp` (Zalos, BSD-3-Clause).

use super::engine::{Engine, InitReply, Media, MemberKey, NegotiatedParams, Out, Role, State, UiEvent};
use serde_json::{json, Value};

#[derive(Default)]
struct FakeMedia {
    negotiated: usize,
    prepared: usize,
    started: usize,
    udp: bool,
    last_params: Option<NegotiatedParams>,
}

impl Media for FakeMedia {
    fn on_negotiated(&mut self, params: &NegotiatedParams) {
        self.negotiated += 1;
        self.last_params = Some(params.clone());
    }
    fn prepare(&mut self, _role: Role) {
        self.prepared += 1;
    }
    fn start(&mut self, _role: Role, _rtp_ser_ip: &str) {
        self.started += 1;
    }
    fn stop(&mut self) {}
    fn send_udp(&self) -> bool {
        self.udp
    }
    fn stats(&self) -> (u64, u64) {
        (7, 9)
    }
}

fn engine(udp: bool) -> Engine<FakeMedia> {
    Engine::new(FakeMedia { udp, ..Default::default() }, json!({"id": 1234, "avatar": "me.png"}))
}

fn codes(out: &[Out]) -> Vec<i64> {
    out.iter().filter_map(|o| match o {
        Out::SendSignal { code, .. } => Some(*code),
        _ => None,
    })
    .collect()
}

fn find_signal(out: &[Out], code: i64) -> Value {
    out.iter()
        .find_map(|o| match o {
            Out::SendSignal { code: c, data } if *c == code => Some(data.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("signal {code} missing"))
}

fn group_servers() -> Value {
    json!([{"rtpaddr": "203.0.113.9|5000", "rtcpaddr": "203.0.113.9|5001"}])
}
const GROUP_SESSION: &str = "sess-0123456789abcdefghijklmnopqrstuv";

#[test]
fn group_outgoing_full_flow() {
    let mut e = engine(false);
    let now = 0.0;
    let out = e.make_call(now, &json!({"type": 5, "groupInfo": {"name": "Nhóm A", "id": "g1"}, "partner": [{"id": "5678", "name": "Lan"}]}));
    assert_eq!(codes(&out), vec![12433]);

    // Server accepted: session + servers + host identity.
    let out = e.handle_recv_signal(now, "12433", &json!({
        "data": json!({"status": 0, "hostCall": 1234, "callId": 900, "groupId": 77, "interval": 5,
                        "session": GROUP_SESSION, "callSetting": json!({"servers": group_servers()}).to_string()})
    }));
    assert_eq!(e.media.negotiated, 1);
    assert_eq!(e.media.prepared, 1);
    let params = e.media.last_params.as_ref().unwrap();
    assert!(params.group && params.host && params.sess_id == GROUP_SESSION);

    // Relay registered → 12434 join + 12098 broadcast; ping scheduled.
    let best = InitReply { server: group_servers()[0].clone(), public_addr: None };
    let out = e.media_prepared(now, Some(best));
    assert_eq!(codes(&out), vec![12434, 12098]);
    let s = find_signal(&out, 12434);
    assert_eq!(s["data"]["data"]["hostCall"], 1234);
    assert_eq!(s["session"], GROUP_SESSION);
    assert!(e.call.as_ref().unwrap().ping_deadline.is_some());

    // The ping fires on the interval and repeats.
    let out = e.tick(now + 5000.0);
    assert_eq!(codes(&out), vec![12097]);
    assert!(e.call.as_ref().unwrap().ping_deadline.unwrap() > now + 5000.0);

    // Roster from the relay: Lan is in the room.
    let out = e.group_roster(now + 5100.0, &[1234, 5678]);
    let member = e
        .call
        .as_ref()
        .unwrap()
        .members
        .iter()
        .find(|(k, _)| *k == MemberKey::of_uid(5678.0))
        .map(|(_, m)| m.clone());
    assert!(member.is_some());
    assert_eq!(member.unwrap().state, "incall");

    // Nobody else? not the case here (Lan is incall) — end the call cleanly.
    let out = e.hangup(now + 6_000.0, "user");
    assert_eq!(codes(&out), vec![12438, 12446]);
    let s = find_signal(&out, 12446);
    assert_eq!(s["status"], 50);
    assert_eq!(s["duration"], 6);
    assert!(!e.in_call());
}

#[test]
fn group_incoming_flow() {
    let mut e = engine(false);
    let now = 0.0;
    let control = json!({"data": {
        "id": 900, "hostCall": 1234, "groupId": 77, "receiverId": 1234, "callType": 0,
        "interval": 5, "groupName": "Nhóm A", "groupAvatar": "g.png",
        "session": GROUP_SESSION,
        "callSetting": json!({"servers": group_servers()}).to_string(),
        "partnerInfo": [{"userId": 5678, "name": "Lan", "callState": 3, "audioState": 0, "videoState": 1}],
    }});
    let out = e.handle_control(now, "group_request", &control);
    assert_eq!(codes(&out), vec![12439]);
    let s = find_signal(&out, 12439);
    assert_eq!(s["status"], 1);
    assert!(matches!(e.call.as_ref().map(|c| (c.group, c.state)), Some((true, State::Incoming))));

    // Accept → prepare → 12436 status 0.
    let out = e.accept(now);
    assert!(out.iter().any(|o| matches!(o, Out::Ui(x) if x.status_code() == "connecting")));
    let out = e.media_prepared(now, Some(InitReply { server: group_servers()[0].clone(), public_addr: None }));
    assert_eq!(codes(&out), vec![12436, 12098]);
    assert_eq!(find_signal(&out, 12436)["status"], 0);

    // Peer ends the group call from the host: everyone left → we finish.
    let out = e.group_roster(now + 1_000.0, &[1234]);
    // Lan left the roster → member left → alone → auto hang-up (finish).
    assert!(codes(&out).contains(&12438) || !e.in_call());
}

#[test]
fn group_incoming_decline_sends_12436_status_3() {
    let mut e = engine(false);
    let now = 0.0;
    e.handle_control(now, "group_request", &json!({"data": {
        "id": 900, "hostCall": 1234, "groupId": 77, "receiverId": 1234, "callType": 0,
        "session": GROUP_SESSION,
        "callSetting": json!({"servers": group_servers()}).to_string(),
    }}));
    let out = e.reject(now, "user");
    assert_eq!(codes(&out), vec![12436]);
    assert_eq!(find_signal(&out, 12436)["status"], 3);
    assert!(!e.in_call());
}

#[test]
fn group_nobody_answered_cancels_with_12437() {
    let mut e = engine(false);
    let now = 0.0;
    e.make_call(now, &json!({"type": 5, "groupInfo": {"name": "Nhóm A", "id": "g1"}, "partner": [{"id": "5678"}]}));
    e.handle_recv_signal(now, "12433", &json!({
        "data": json!({"status": 0, "hostCall": 1234, "callId": 900, "session": GROUP_SESSION,
                        "callSetting": json!({"servers": group_servers()}).to_string()})
    }));
    e.media_prepared(now, None);
    assert!(e.in_call());

    // 60 s later nobody joined → 12437 cancel + finish (không 12438/12446).
    let out = e.tick(now + 61_000.0);
    assert_eq!(codes(&out), vec![12097, 12437]);
    assert!(!e.in_call());
}

#[test]
fn group_kicked_by_server() {
    let mut e = engine(false);
    let now = 0.0;
    e.make_call(now, &json!({"type": 5, "groupInfo": {"name": "Nhóm A", "id": "g1"}, "partner": [{"id": "5678"}]}));
    e.handle_recv_signal(now, "12433", &json!({
        "data": json!({"status": 0, "hostCall": 1234, "callId": 900, "session": GROUP_SESSION,
                        "callSetting": json!({"servers": group_servers()}).to_string()})
    }));
    e.media_prepared(now, None);
    let out = e.group_ended(now + 1_000.0, "kicked");
    assert_eq!(codes(&out), vec![12438, 12446]);
    assert!(out.iter().any(|o| matches!(o, Out::Ui(x) if x.status_code() == "groupKicked")));
    assert!(!e.in_call());
}

#[test]
fn group_ping_failure_finishes_when_media_dead() {
    let mut e = engine(false);
    let now = 0.0;
    e.make_call(now, &json!({"type": 5, "groupInfo": {"name": "Nhóm A", "id": "g1"}, "partner": [{"id": "5678"}]}));
    e.handle_recv_signal(now, "12433", &json!({
        "data": json!({"status": 0, "hostCall": 1234, "callId": 900, "session": GROUP_SESSION,
                        "callSetting": json!({"servers": group_servers()}).to_string()})
    }));
    e.media_prepared(now, None);
    let out = e.handle_recv_signal(now + 1_000.0, "12097", &json!({"data": json!({"error": -1, "message": "gone"}).to_string()}));
    assert_eq!(codes(&out), vec![12438, 12446]);
    assert!(!e.in_call());
}

#[test]
fn group_answer_on_other_device_finishes_silently() {
    let mut e = engine(false);
    let now = 0.0;
    e.handle_control(now, "group_request", &json!({"data": {
        "id": 900, "hostCall": 1234, "receiverId": 1234, "callType": 0,
        "session": GROUP_SESSION,
        "callSetting": json!({"servers": group_servers()}).to_string(),
    }}));
    let out = e.handle_control(now, "group_answer", &json!({"data": {}}));
    // Answered elsewhere: finish(0) without join signals.
    assert_eq!(codes(&out), Vec::<i64>::new());
    assert!(!e.in_call());
}
