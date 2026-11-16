//! State-machine tests for the 1-1 voice engine: full call flows against a
//! fake media backend and an injected clock. The expected signal shapes mirror
//! `engine_core.cpp` (Zalos, BSD-3-Clause).

use super::engine::{Engine, InitReply, Media, NegotiatedParams, Out, Role, State, UiEvent};
use serde_json::{json, Value};

/// Records everything the engine asks of the media backend.
#[derive(Default)]
struct FakeMedia {
    negotiated: usize,
    prepared: usize,
    started: usize,
    stopped: usize,
    udp: bool,
}

impl Media for FakeMedia {
    fn on_negotiated(&mut self, _params: &NegotiatedParams) {
        self.negotiated += 1;
    }
    fn prepare(&mut self, _role: Role) {
        self.prepared += 1;
    }
    fn start(&mut self, _role: Role, _rtp_ser_ip: &str) {
        self.started += 1;
    }
    fn stop(&mut self) {
        self.stopped += 1;
    }
    fn send_udp(&self) -> bool {
        self.udp
    }
    fn stats(&self) -> (u64, u64) {
        (11, 22)
    }
}

fn engine(udp: bool) -> Engine<FakeMedia> {
    let mut e = Engine::new(FakeMedia { udp, ..Default::default() }, json!({"id": 1234, "avatar": "me.png"}));
    // Local uid only matters once the 401 reply arrives; the init payload covers the fallback.
    let _ = &e;
    e
}

fn codes(out: &[Out]) -> Vec<i64> {
    out.iter().filter_map(|o| match o {
        Out::SendSignal { code, .. } => Some(*code),
        _ => None,
    })
    .collect()
}

fn updates(out: &[Out]) -> Vec<(String, Value)> {
    out.iter()
        .filter_map(|o| match o {
            Out::Update { command, data } => Some((command.clone(), data.clone())),
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
        .unwrap_or_else(|| panic!("signal {code} missing in {out:?}"))
}

fn server_obj() -> Value {    json!({"rtpaddr": "203.0.113.9|5000", "rtcpaddr": "203.0.113.9|5001", "sessId": "sess-1"})
}

fn server_reply() -> Value {
    json!({"servers": [{"rtpaddr": "203.0.113.9|5000", "rtcpaddr": "203.0.113.9|5001"}], "sessId": "sess-0123456789abcdefghijklmnopqrstuv", "fromId": 1234, "toId": 5678})
}

#[test]
fn outgoing_flow_to_declined() {
    let mut e = engine(false);
    let now = 1_000.0;
    let out = e.make_call(now, &json!({"type": 1, "partner": [{"id": "5678", "name": "Lan"}]}));
    assert_eq!(codes(&out), vec![401]);
    assert_eq!(updates(&out)[0], ("callState".into(), json!({"state": "incall"})));
    let s401 = find_signal(&out, 401);
    assert_eq!(s401["calleeId"], "5678");
    assert_eq!(s401["codec"], "[]\n");

    // The reply carries the media servers; engine negotiates and prepares.
    let out = e.handle_recv_signal(now, "401", &server_reply());
    assert_eq!(e.media.negotiated, 1);
    assert_eq!(e.media.prepared, 1);
    assert!(out.is_empty());

    // Relay registration picked server 2 of 2: 416 with its addresses + extendData.
    let best = InitReply {
        server: json!({"rtpaddr": "198.51.100.7|6000", "rtcpaddr": "198.51.100.7|6001"}),
        public_addr: Some("1.2.3.4|1234".into()),
    };
    let out = e.media_prepared(now, Some(best));
    assert_eq!(codes(&out), vec![416]);
    let s416 = find_signal(&out, 416);
    assert_eq!(s416["rtpAddress"], "198.51.100.7|6000");
    let ext: Value = serde_json::from_str(s416["extendData"].as_str().unwrap()).unwrap();
    assert_eq!(ext["newZrtc"], 1);
    assert_eq!(ext["srtpMode"], 1);
    assert_eq!(ext["p2p"][0]["ip"], "1.2.3.4");
    assert_eq!(ext["serverAddr"][0]["rtp"], "198.51.100.7|6000");
    assert_eq!(ext["video"]["codec"][0]["name"], "h264");

    // Peer is ringing.
    let out = e.handle_control(now, "ring_ring", &json!({}));
    assert!(out.iter().any(|o| matches!(o, Out::Ui(x) if *x == super::engine::UiEvent::Status("ringing".into()))));

    // Peer declines: answer status 3 → finish 160 with the 406 log.
    let out = e.handle_control(now, "answer", &json!({"data": {"params": "{\"status\": 3}"}}));
    assert_eq!(codes(&out), vec![406]);
    let s406 = find_signal(&out, 406);
    assert_eq!(s406["status"], 160);
    assert_eq!(s406["role"], true); // caller
    assert_eq!(updates(&out).last().unwrap().1["state"], "free");
    assert!(e.media.stopped >= 1);
    assert!(!e.in_call());
}

#[test]
fn outgoing_flow_answered() {
    let mut e = engine(false);
    let now = 1_000.0;
    e.make_call(now, &json!({"type": 1, "partner": [{"id": "5678"}]}));
    e.handle_recv_signal(now, "401", &server_reply());
    e.media_prepared(now, None); // no relay answered yet, but sendUdp is off → proceed with first
    let out = e.handle_control(now, "answer", &json!({"data": {"params": "{\"status\": 0}", "rtpSerIp": "9.9.9.9"}}));
    assert_eq!(codes(&out), vec![408]);
    assert_eq!(e.media.started, 1);
    assert!(matches!(e.call.as_ref().map(|c| c.state), Some(State::Connected)));

    // Peer ends: 50 (connected-ended) + 406 with duration ≥ 0.
    let out = e.handle_control(now + 5_000.0, "end_call", &json!({"data": {"callId": e.call.as_ref().unwrap().call_id}}));
    assert_eq!(codes(&out), vec![406]);
    assert_eq!(find_signal(&out, 406)["status"], 50);
    assert!(find_signal(&out, 406)["duration"].as_i64().unwrap() >= 5);
}

#[test]
fn incoming_flow_accept_and_end() {
    let mut e = engine(false);
    let now = 1_000.0;
    let request = json!({
        "uidN": "5678", "uidFrom": 5678, "uidTo": 1234, "callId": 42,
        "rtpAddress": "203.0.113.9|5000", "rtcpAddress": "203.0.113.9|5001",
        "session": "sess-incoming",
        "params": {"Dname": "Lan", "avatar": "a.png", "extendData": "{\"callType\": 0, \"serverAddr\": [{\"rtpIPv6\": \"\", \"rtcpIPv6\": \"\"}]}"}
    });
let out = e.handle_control(now, "request", &json!({"data": request}));
    assert_eq!(codes(&out), vec![407]);
    assert_eq!(updates(&out)[0], ("callState".into(), json!({"state": "incall"})));
    assert!(out.iter().any(|o| matches!(o, Out::Request { command, .. } if command == "getAliasName")));
    assert!(matches!(&e.call, Some(c) if c.role == Role::Callee && c.state == State::Incoming));

    // Accept: media negotiated + prepared; the 402 goes out with the relay result.
    let out = e.accept(now);
    assert_eq!(e.media.negotiated, 1);
    assert_eq!(e.media.prepared, 1);
    assert!(out.iter().any(|o| matches!(o, Out::Ui(x) if *x == super::engine::UiEvent::Status("connecting".into()))));
    let out = e.media_prepared(now, Some(InitReply { server: server_obj(), public_addr: None }));
    assert_eq!(codes(&out), vec![402]);
    let s402 = find_signal(&out, 402);
    assert_eq!(s402["status"], 0);
    assert_eq!(s402["rtpAddress"], "203.0.113.9|5000");
    assert_eq!(s402["session"], "sess-incoming");
    let ext: Value = serde_json::from_str(s402["extendData"].as_str().unwrap()).unwrap();
    assert_eq!(ext["sP2P"], 1);
    assert_eq!(ext["select2side"], 1);
    assert_eq!(e.media.started, 1);

    // Peer hangs up: notification + 406 with the stats the server expects.
    let out = e.handle_control(now + 2_000.0, "end_call", &json!({"data": {"callId": 42}}));
    assert!(out.iter().any(|o| matches!(o, Out::Request { command, .. } if command == "showNotification")));
    assert_eq!(codes(&out), vec![406]);
    let s406 = find_signal(&out, 406);
    assert_eq!(s406["status"], 50);
    assert_eq!(s406["role"], false); // callee
    let stats: Value = serde_json::from_str(s406["data"].as_str().unwrap()).unwrap();
    assert_eq!(stats["RxTotalPkt"], 11);
    assert_eq!(stats["TxTotalPkt"], 22);
    assert_eq!(stats["sysInfo"], "native-engine");
    assert!(!e.in_call());

    // The same request again (delayed duplicate) is dropped by the ended-call cache.
    let out = e.handle_control(now + 2_100.0, "request", &json!({"data": request}));
    assert!(out.is_empty());
}

#[test]
fn incoming_reject_sends_402_status_3() {
    let mut e = engine(false);
    let now = 1_000.0;
e.handle_control(now, "request", &json!({"data": {"uidN": "5678", "uidFrom": 5678, "uidTo": 1234, "callId": 7, "params": {}}}));
    let out = e.reject(now, "user");
    assert_eq!(codes(&out), vec![402, 406]);
    assert_eq!(find_signal(&out, 402)["status"], 3);
    assert_eq!(find_signal(&out, 402)["extendData"], "{}\n");
    assert_eq!(find_signal(&out, 406)["status"], 160);
}

#[test]
fn setup_and_ring_timeouts() {
    let mut e = engine(false);
    let now = 0.0;
    e.make_call(now, &json!({"type": 1, "partner": [{"id": "5678"}]}));
    // Before the deadline nothing happens.
    assert!(e.tick(now + 14_999.0).is_empty());
    let out = e.tick(now + 15_000.0);
    assert_eq!(codes(&out), vec![405, 406]);
    assert_eq!(find_signal(&out, 406)["status"], 103);
    assert!(out.iter().any(|o| matches!(o, Out::Ui(x) if *x == super::engine::UiEvent::Status("recipientUnreachable".into()))));

    // Ring timeout once the peer rang.
    let mut e = engine(false);
    let now = 0.0;
    e.make_call(now, &json!({"type": 1, "partner": [{"id": "5678"}]}));
    e.handle_recv_signal(now, "401", &server_reply());
    e.media_prepared(now, None);
    e.handle_control(now, "ring_ring", &json!({}));
    assert!(e.tick(now + 44_999.0).is_empty());
    let out = e.tick(now + 45_000.0);
    assert_eq!(codes(&out), vec![405, 406]);
    assert!(out.iter().any(|o| matches!(o, Out::Ui(x) if *x == super::engine::UiEvent::Status("recipientNoAnswer".into()))));
}

#[test]
fn recv_signal_401_without_servers_fails() {
    let mut e = engine(false);
    let now = 0.0;
    e.make_call(now, &json!({"type": 1, "partner": [{"id": "5678"}]}));
    let out = e.handle_recv_signal(now, "401", &json!({"status": 1, "msg": "busy"}));
    assert_eq!(codes(&out), vec![406]);
    assert_eq!(find_signal(&out, 406)["status"], 103);
}

#[test]
fn incoming_while_busy_auto_declines_new_call() {
    let mut e = engine(false);
    let now = 0.0;
    e.make_call(now, &json!({"type": 1, "partner": [{"id": "5678"}]}));
    // Đang bận: cuộc mới bị auto-decline (402 status 3), cuộc cũ nguyên vẹn.
    let out = e.handle_control(now, "request", &json!({"data": {"uidN": "9999", "callId": 55, "params": {}}}));
    assert_eq!(codes(&out), vec![402]);
    assert_eq!(find_signal(&out, 402)["status"], 3);
    assert_eq!(find_signal(&out, 402)["callId"], 55);
    assert_eq!(e.call.as_ref().unwrap().peer_id, "5678");
}

#[test]
fn other_device_answer_ends_our_incoming() {
    let mut e = engine(false);
    let now = 0.0;
e.handle_control(now, "request", &json!({"data": {"uidN": "5678", "uidFrom": 5678, "uidTo": 1234, "callId": 42, "params": {}}}));
    let out = e.handle_control(now, "answer", &json!({"data": {"callId": 42}}));
    assert_eq!(codes(&out), Vec::<i64>::new()); // silent finish(0): no 406 for a multi-device takeover
    assert!(!e.in_call());
    assert!(updates(&out).last().unwrap().1["state"] == "free");
}

#[test]
fn server_unreachable_on_prepare_fails_the_call() {
    let mut e = engine(true); // sendUdp = true, no relay answered
    let now = 0.0;
e.handle_control(now, "request", &json!({"data": {"uidN": "5678", "uidFrom": 5678, "uidTo": 1234, "callId": 42, "session": "s", "rtpAddress": "x", "rtcpAddress": "y", "params": {}}}));
    e.accept(now);
    let out = e.media_prepared(now, None);
    // The 402 still goes out (matches the original); the call was already
    // "connected", so the hang-up is a proper 409 endcall, then the 406 log.
    assert_eq!(codes(&out), vec![402, 409, 406]);
    assert!(out.iter().any(|o| matches!(o, Out::Ui(x) if *x == super::engine::UiEvent::Status("serverUnreachable".into()))));
    assert!(!e.in_call());
}

#[test]
fn group_call_outgoing_signals_12433() {
    let mut e = engine(false);
    let now = 0.0;
    let out = e.make_call(now, &json!({"type": 5, "groupInfo": {"name": "Nhóm A", "id": "g1"}, "partner": [{"id": "5678", "name": "Lan"}]}));
    assert_eq!(codes(&out), vec![12433]);
    let s = find_signal(&out, 12433);
    assert_eq!(s["data"]["groupName"], "Nhóm A");
    assert_eq!(s["data"]["noiseId"], json!(["5678"]));
    assert!(out.iter().any(|o| matches!(o, Out::Ui(x) if x.status_code() == "groupCalling")));
    assert_eq!(updates(&out)[0], ("callState".into(), json!({"state": "incall"})));
    assert!(e.in_call());
}



#[test]
fn incoming_unanswered_times_out_with_405() {
    let mut e = engine(false);
    let now = 1_000.0;
    e.handle_control(now, "request", &json!({"data": {"uidN": "5678", "uidFrom": 5678, "uidTo": 1234, "callId": 42, "params": {}}}));
    assert!(e.in_call());
    // Chưa tới 45 s: im lặng.
    assert!(e.tick(now + 44_999.0).is_empty());
    // Quá 45 s không ai bắt: 405 báo cho bên gọi + dọn state (banner đã ẩn).
    let out = e.tick(now + 45_000.0);
    assert_eq!(codes(&out), vec![405, 406]);
    assert_eq!(find_signal(&out, 405)["callId"], 42);
    assert!(!e.in_call());
}

#[test]
fn incoming_retransmit_same_id_is_silent() {
    let mut e = engine(false);
    let now = 1_000.0;
    e.handle_control(now, "request", &json!({"data": {"uidN": "5678", "callId": 42, "params": {}}}));
    assert!(e.in_call());
    // Retransmit cùng callId → bỏ qua lặng (không spam 402).
    assert!(e.handle_control(now + 100.0, "request", &json!({"data": {"uidN": "5678", "callId": 42, "params": {}}})).is_empty());
    assert!(matches!(&e.call, Some(c) if c.call_id == 42.0 && c.state == State::Incoming));
}
