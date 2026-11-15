//! Golden-vector tests for the ZRTC wire builders — the Rust port of
//! `test_call_wire.cpp::zrtp()` (Zalos, BSD-3-Clause). Every expected byte
//! string comes from `testdata/zrtp.json`, so any deviation from the wire
//! format Zalo's relay expects fails here first.

use super::wire::*;
use serde_json::{json, Value};

const VECTORS: &str = include_str!("testdata/zrtp.json");

fn vectors() -> Value {
    serde_json::from_str(VECTORS).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sess() -> Vec<u8> {
    let mut s = b"sess-".to_vec();
    s.extend(std::iter::repeat(b'x').take(40));
    s
}

#[test]
fn media_wrapper_and_control_header() {
    let v = vectors();
    assert_eq!(hex(&wrap_media_packet(msg_type::AUDIO_RTP, 0xfeedbeef, b"abc")), v["media"]);
    let h = ControlHeader {
        msg_type: msg_type::SERVER_REPLY,
        magic: 9,
        field02: 5,
        seq: 6,
        uid: 7,
        token: 8,
        cmd: 300,
        sub_cmd: 260,
    };
    assert_eq!(hex(&build_control_header(&h)), v["header"]);
}

#[test]
fn init_zrtp_caller_and_callee() {
    let v = vectors();
    assert_eq!(hex(&build_init_zrtp_packet(true, 11, 22, 33, &sess())), v["initCaller"]);
    assert_eq!(hex(&build_init_zrtp_packet_with(false, 11, 22, 33, &sess(), b"ex", 0x02)), v["initCallee"]);
}

#[test]
fn group_init_layouts() {
    let v = vectors();
    for entry in v["groupInit"].as_array().unwrap() {
        let host = entry["host"].as_bool().unwrap();
        assert_eq!(
            hex(&build_group_init_zrtp_packet(host, 5, &sess(), host, 77, 88, entry["layout"].as_str().unwrap())),
            entry["out"],
            "layout {}",
            entry["layout"]
        );
    }
}

#[test]
fn share_ping_endcall_zavi() {
    let v = vectors();
    assert_eq!(hex(&build_share_init_zrtp_packet(99, &sess())), v["shareInit"]);
    assert_eq!(hex(&build_share_zavi_ping_packet(99, 4, &sess())), v["sharePing"]);
    let zavi = v["zaviPing"].as_array().unwrap();
    assert_eq!(hex(&build_zavi_ping_packet(3, 4, &sess(), true)), zavi[0]);
    assert_eq!(hex(&build_zavi_ping_packet(3, 4, &sess(), false)), zavi[1]);
    assert_eq!(hex(&build_ping_packet(3, 4, &sess())), v["ping"]);
    assert_eq!(hex(&build_end_call_packet(3, 4, 0x8000_0001)), v["endCall"]);
}

#[test]
fn req_forward_and_video_quality() {
    let v = vectors();
    assert_eq!(hex(&build_req_forward_packet(3, 4, 9, 10, &req_forward_status(49, 2, 3))), v["reqForward"]);
    assert_eq!(
        hex(&build_group_vid_qual_req(
            3,
            4,
            1,
            &[VidQualEntry { uid: 10, quality_id: 0xff }, VidQualEntry { uid: 11, quality_id: 3 }]
        )),
        v["vidQualReq"]
    );
    assert_eq!(
        hex(&build_group_vid_qual_config(
            3,
            4,
            2,
            &[
                VidLayer { id: 0, a: 300, b: 720, c: 360 },
                VidLayer { id: 4, a: 70000, b: 1280, c: 720 },
            ]
        )),
        v["vidQualCfg"]
    );
    assert_eq!(hex(&build_group_sync_packet(3, 4, 5, 14)), v["groupSync"]);
}

#[test]
fn p2p_framing() {
    let v = vectors();
    assert_eq!(hex(&build_p2p_bind(true, 0x0102_0304, 70000, 2)), v["p2pBind"]);
    assert_eq!(hex(&build_p2p_pong(false, &unhex("7f00010403020104000100"))), v["p2pPong"]);
    assert_eq!(hex(&wrap_p2p_audio(false, 5, &unhex("80"))), v["p2pAudio"]);
    assert_eq!(hex(&wrap_p2p_video(true, 5, &unhex("80"))), v["p2pVideo"]);
    let rtcp = v["p2pRtcp"].as_array().unwrap();
    assert_eq!(hex(&wrap_p2p_rtcp(true, 5, true, &unhex("81"))), rtcp[0]);
    assert_eq!(hex(&wrap_p2p_rtcp(true, 5, false, &unhex("81"))), rtcp[1]);
    assert_eq!(
        hex(&build_p2p_bind_answer(false, 0x0102_0304, &unhex("7f000004030201020007000200000001"), b"10.0.0.5|4200")),
        v["p2pAnswer"]
    );
}

#[test]
fn unwrap_packets() {
    let v = vectors();
    for entry in v["unwrap"].as_array().unwrap() {
        let parsed = unwrap_packet(&unhex(entry["in"].as_str().unwrap())).expect("unwrap must parse");
        assert_eq!(unwrapped_json(&parsed), entry["out"], "vector in={}", entry["in"]);
    }
}

#[test]
fn room_updated_roster() {
    let v = vectors();
    for entry in v["room"].as_array().unwrap() {
        let roster = parse_room_updated(&unhex(entry["in"].as_str().unwrap())).expect("room parse");
        let got: Vec<Value> = roster.iter().map(|u| json!(u)).collect();
        assert_eq!(Value::Array(got), entry["out"]);
    }
}

#[test]
fn unwrap_truncated_media_and_unknown() {
    // The unwrap vectors cover the happy paths; these pin the defensive branches.
    let truncated = unwrap_packet(&unhex("03efbe"));
    let p = truncated.expect("parse");
    assert!(p.is_media && p.truncated && p.raw == vec![0x03, 0xef, 0xbe]);
    assert_eq!(unwrap_packet(&[]), None);
    let unknown = unwrap_packet(&unhex("5500")).expect("parse");
    assert!(!unknown.is_media && !unknown.is_control && unknown.raw == vec![0x55, 0x00]);
}
