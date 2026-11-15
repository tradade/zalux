//! Golden-vector tests for the RTP/RTCP builders — the Rust port of
//! `test_call_wire.cpp::rtp()` and `::rtcp()` (Zalos, BSD-3-Clause).

use super::rtp::*;
use super::rtcp::*;
use crate::zrtc::wire::unhex;
use serde_json::{json, Value};

fn vectors(name: &str) -> Value {
    let text = match name {
        "rtp" => include_str!("testdata/rtp.json"),
        "rtcp" => include_str!("testdata/rtcp.json"),
        _ => unreachable!(),
    };
    serde_json::from_str(text).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn args_of(a: &Value) -> RtpHeaderArgs {
    // C++ `toInt()/toDouble()` default missing keys to 0; mirror that.
    let num = |k: &str| a.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    RtpHeaderArgs {
        seq: num("seq") as u16,
        ts: num("ts") as u32,
        ssrc: num("ssrc") as u32,
        tw_seq: num("twSeq") as u16,
        marker: a.get("marker").and_then(Value::as_bool).unwrap_or(false),
        pt: a.get("pt").map(|v| num("pt") as i32).unwrap_or(AUDIO_PT),
        layer_seq: num("layerSeq") as u16,
        capture_ms: num("captureMs"),
        send_ms: num("sendMs"),
        level: a.get("level").map(|v| num("level") as i32).unwrap_or(127),
    }
}

#[test]
fn rtp_headers_match_vectors() {
    let v = vectors("rtp");
    for e in v["audio"].as_array().unwrap() {
        assert_eq!(hex(&build_audio_rtp_header(&args_of(&e["args"]))), e["out"], "audio");
    }
    for e in v["groupAudio"].as_array().unwrap() {
        assert_eq!(hex(&build_group_audio_rtp_header(&args_of(&e["args"]))), e["out"], "groupAudio");
    }
    for e in v["groupVideo"].as_array().unwrap() {
        let a = &e["args"];
        // The C++ test defaults missing keys: toBool()=false, toInt()=0.
        let sfu = SfuExt {
            long_form: a["sfu"].get("long").and_then(Value::as_bool).unwrap_or(false),
            id: a["sfu"].get("id").and_then(Value::as_i64).unwrap_or(0) as i32,
        };
        assert_eq!(hex(&build_group_video_rtp_header(&args_of(a), &sfu)), e["out"], "groupVideo");
    }
    for e in v["levels"].as_array().unwrap() {
        // The C++ test appends one padding byte before parsing.
        let mut rtp = unhex(e["rtp"].as_str().unwrap());
        rtp.push(0);
        let level = rtp_audio_level(&rtp).map(Value::from).unwrap_or(Value::Null);
        assert_eq!(level, e["level"]);
    }
}

#[test]
fn audio_header_round_trip() {
    let header = build_audio_rtp_header(&RtpHeaderArgs {
        seq: 4242,
        ts: 320_000,
        ssrc: 0xdead_beef,
        tw_seq: 77,
        marker: false,
        ..Default::default()
    });
    let f = read_audio_rtp_header(&header).expect("parse");
    assert_eq!(f.pt, AUDIO_PT);
    assert_eq!(f.seq, 4242);
    assert_eq!(f.ssrc, 0xdead_beef);
    assert_eq!(f.tw_seq, Some(77));
    assert_eq!(read_tw_seq(&header), Some(77));
}

#[test]
fn twcc_recorder_matches_vectors() {
    let v = vectors("rtcp");
    let mut rec = TwccRecorder::new();
    let adds = v["adds"].as_array().unwrap();
    let builds = v["builds"].as_array().unwrap();
    let mut bi = 0usize;
    for i in 0..=adds.len() {
        while bi < builds.len() && builds[bi]["after"].as_i64().unwrap() == i as i64 {
            let got: Vec<String> = rec.build(111, 222).iter().map(|p| hex(p)).collect();
            let want: Vec<String> = builds[bi]["out"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
            assert_eq!(got, want, "build #{bi}");
            bi += 1;
        }
        if i < adds.len() {
            let a = &adds[i];
            rec.add(a[0].as_i64().unwrap() as u32, a[1].as_f64().unwrap());
        }
    }
    assert_eq!(bi, builds.len(), "all builds consumed");
}

#[test]
fn twcc_parse_matches_vectors() {
    let v = vectors("rtcp");
    for e in v["parsed"].as_array().unwrap() {
        let fb = parse_twcc(&unhex(e["pkt"].as_str().unwrap())).expect("parse");
        let w = &e["fb"];
        assert_eq!(fb.base, w["base"].as_i64().unwrap() as u16);
        assert_eq!(fb.count, w["count"].as_i64().unwrap() as u16);
        assert_eq!(fb.ref_units, w["refUnits"].as_u64().unwrap() as u32);
        assert_eq!(fb.fb_count, w["fbCount"].as_i64().unwrap() as u8);
        let st: Vec<Value> = fb.status.iter().map(|s| json!(s)).collect();
        let dq: Vec<Value> = fb.delta_q.iter().map(|d| json!(d)).collect();
        assert_eq!(Value::Array(st), w["status"]);
        assert_eq!(Value::Array(dq), w["deltaQ"]);
    }
}

#[test]
fn sr_rr_sdes_pli_nack_match_vectors() {
    let v = vectors("rtcp");
    let misc = &v["misc"];
    let blk = ReportBlock {
        ssrc: 5,
        fraction_lost: 12,
        cum_lost: -3,
        ext_high_seq: 70000,
        jitter: 33,
        lsr: 0xdead_beef,
        dlsr: 99,
    };
    assert_eq!(hex(&build_sr(0x0102_0304, 1_700_000_000_123.5, 99, 5, 600, &[blk])), misc["sr"]);
    assert_eq!(hex(&build_sr(7, 1_700_000_000_999.0, 1, 2, 3, &[])), misc["srEmpty"]);
    assert_eq!(hex(&build_rr(9, &[blk])), misc["rr"]);
    assert_eq!(hex(&build_sdes(0xabcd_ef01)), misc["sdes"]);
    assert_eq!(hex(&build_pli(1, 2)), misc["pli"]);
    assert_eq!(
        hex(&build_nack(1, 2, &[65534, 65535, 0, 3, 17, 18, 40])),
        misc["nack"]
    );
}

#[test]
fn nack_tracker_matches_vectors() {
    let v = vectors("rtcp");
    let mut nt = NackTracker::default();
    for e in v["nacks"].as_array().unwrap() {
        let out = nt.note(e["ssrc"].as_u64().unwrap() as u32, e["seq"].as_i64().unwrap() as u16, e["now"].as_f64().unwrap());
        let got: Vec<Value> = out
            .iter()
            .map(|r| json!({ "ssrc": r.ssrc, "seqs": r.seqs.iter().map(|s| json!(s)).collect::<Vec<_>>() }))
            .collect();
        assert_eq!(Value::Array(got), e["out"]);
    }
}

#[test]
fn rtcp_feedback_matches_vectors() {
    let v = vectors("rtcp");
    let mut fb = RtcpFeedback::new(4242);
    for e in v["feed"].as_array().unwrap() {
        let video = e["kind"] == "video";
        let now = e["now"].as_f64().unwrap();
        fb.on_rtp(video, &unhex(e["rtp"].as_str().unwrap()), now);
        if let Some(rtcp) = e.get("rtcp") {
            fb.on_rtcp(false, &unhex(rtcp.as_str().unwrap()), now);
            fb.on_rtcp(true, &unhex(rtcp.as_str().unwrap()), now);
        }
        if let Some(want) = e.get("twcc") {
            let got: Vec<String> = fb.twcc_packets().iter().map(|p| hex(p)).collect();
            let want: Vec<String> = want.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
            assert_eq!(got, want);
        }
        if let Some(w) = e.get("audioReport") {
            let i = u32::from_str_radix(&e["rtp"].as_str().unwrap()[8..16], 16).unwrap() / 960;
            assert_eq!(
                hex(&fb.audio_report(now, i * 320, i, i * 40)),
                w.as_str().unwrap()
            );
            let vr = fb.video_report(now).map(|b| Value::from(hex(&b))).unwrap_or(Value::Null);
            assert_eq!(vr, e["videoReport"].clone());
            let p = fb.pli().map(|b| Value::from(hex(&b))).unwrap_or(Value::Null);
            assert_eq!(p, e["pli"].clone());
        }
    }
    let c = &v["counters"];
    let got = fb.counters();
    assert_eq!(got.twcc, c["twcc"].as_i64().unwrap() as i32);
    assert_eq!(got.reports, c["reports"].as_i64().unwrap() as i32);
    assert_eq!(got.pli, c["pli"].as_i64().unwrap() as i32);
}
