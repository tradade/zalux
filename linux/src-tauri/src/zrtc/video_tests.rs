//! Golden-vector tests for the H.264 wire layer — the Rust port of
//! `test_call_wire.cpp::video()` (Zalos, BSD-3-Clause).

use super::video::*;
use crate::zrtc::wire::unhex;
use serde_json::{json, Value};
use std::cell::Cell;

thread_local! {
    static CLOCK: Cell<f64> = const { Cell::new(0.0) };
}

fn now() -> f64 {
    CLOCK.with(|c| c.get())
}

fn set_clock(v: f64) {
    CLOCK.with(|c| c.set(v));
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn vectors() -> Value {
    serde_json::from_str(include_str!("testdata/video.json")).unwrap()
}

#[test]
fn sps_sizes_match_vectors() {
    let v = vectors();
    for e in v["sps"].as_array().unwrap() {
        let got = sps_size(&unhex(e["in"].as_str().unwrap()));
        if e["out"].is_null() {
            assert!(got.is_none(), "sps {} should not parse", e["in"]);
        } else {
            let s = got.unwrap_or_else(|| panic!("sps {} should parse", e["in"]));
            assert_eq!(json!({"width": s.width, "height": s.height}), e["out"], "sps {}", e["in"]);
        }
    }
}

#[test]
fn nal_units_codec_and_packetizer() {
    let v = vectors();
    let start: &[u8] = &[0, 0, 0, 1];
    let nal: Vec<Vec<u8>> = v["nal"].as_array().unwrap().iter().map(|n| unhex(n.as_str().unwrap())).collect();
    let mut key = Vec::new();
    for n in &nal {
        key.extend_from_slice(start);
        key.extend_from_slice(n);
    }
    assert_eq!(avc_codec_string(&key), v["codec"].as_array().unwrap()[0]);
    // No SPS → empty codec string.
    assert!(avc_codec_string(&unhex("000001650000000000")).is_empty());
    // Trailing zero bytes are trimmed when splitting NALs.
    let mut padded = key.clone();
    padded.extend_from_slice(&[0, 0, 0]);
    let got: Vec<String> = nal_units(&padded).iter().map(|u| hex(u)).collect();
    let want: Vec<String> = v["nal"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect();
    assert_eq!(got, want);

    let pk: Vec<String> = packetize_frame(&key, MAX_CHUNK).iter().map(|p| hex(p)).collect();
    let want: Vec<String> = v["packets"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect();
    assert_eq!(pk, want);

    let small: Vec<String> = packetize_frame(&unhex("00000165aa"), MAX_CHUNK).iter().map(|p| hex(p)).collect();
    let want: Vec<String> = v["small"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect();
    assert_eq!(small, want);

    let exact: Vec<usize> = packetize_frame(&vec![b'x'; 2136], MAX_CHUNK).iter().map(|p| p.len()).collect();
    let want: Vec<usize> = v["exact"].as_array().unwrap().iter().map(|x| x.as_i64().unwrap() as usize).collect();
    assert_eq!(exact, want);
}

fn run_assembler(group: bool) {
    let v = vectors();
    let a = if group { &v["groupAsm"] } else { &v["asm"] };
    set_clock(5000.0);
    let frames = std::rc::Rc::new(std::cell::RefCell::new(Vec::<Value>::new()));
    let gaps = std::rc::Rc::new(std::cell::Cell::new(0i32));
    let frames_cb = frames.clone();
    let gaps_cb = gaps.clone();
    let mut asmb = VideoAssembler::new(
        Box::new(move |f: VideoFrame| {
            // The vectors store `at` as an integer.
            frames_cb.borrow_mut().push(json!({"key": f.key, "data": hex(&f.data), "ts": f.ts, "at": now() as i64}));
        }),
        now,
        group,
        Box::new(move || gaps_cb.set(gaps_cb.get() + 1)),
    );
    for st in a["steps"].as_array().unwrap() {
        let p = &st["pkt"];
        set_clock(st.get("now").and_then(Value::as_f64).unwrap_or(0.0));
        let num = |k: &str| p.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        asmb.push(VideoPacket {
            ts: num("ts") as u32,
            pt: num("pt") as i32,
            marker: p.get("marker").and_then(Value::as_bool).unwrap_or(false),
            payload: unhex(p["payload"].as_str().unwrap_or("")),
            seq: num("seq") as u16,
            roc: num("roc") as u32,
            ssrc: 0,
        });
    }
    let want_frames = a["frames"].as_array().unwrap();
    let got_frames = frames.borrow().clone();
    assert_eq!(got_frames.len(), want_frames.len(), "{} frame count", if group { "group" } else { "1-1" });
    for (got, w) in got_frames.iter().zip(want_frames) {
        assert_eq!(got, w, "frame");
    }
    if let Some(want_gaps) = a["gaps"].as_i64() {
        assert_eq!(gaps.get(), want_gaps as i32, "gaps");
    }
    if let Some(stats) = a["stats"].as_object() {
        assert_eq!(asmb.stats.frames, stats["frames"].as_i64().unwrap() as i32, "stats.frames");
        assert_eq!(asmb.stats.dropped, stats["dropped"].as_i64().unwrap() as i32, "stats.dropped");
        assert_eq!(asmb.stats.keys, stats["keys"].as_i64().unwrap() as i32, "stats.keys");
        assert_eq!(asmb.stats.gaps, stats["gaps"].as_i64().unwrap() as i32, "stats.gaps");
    }
    if group {
        if let Some(nb) = a["nalBytes"].as_object() {
            for (k, want) in nb {
                assert_eq!(asmb.nal_bytes.get(&(k.parse::<i32>().unwrap())).copied().unwrap_or(0), want.as_i64().unwrap(), "nalBytes[{k}]");
            }
        }
    }
}

#[test]
fn assembler_matches_vectors_1_1() {
    run_assembler(false);
}

#[test]
fn assembler_matches_vectors_group() {
    run_assembler(true);
}

/// Encoder thật → bitstream Annex-B → renderer ra JPEG (SOI magic + throttle
/// 500 ms). Không cần golden file: encoder OpenH264 bundle sinh tại chỗ.
#[test]
fn renderer_decodes_encoder_stream_to_jpeg() {
    use openh264::encoder::{Encoder, EncoderConfig};
    use openh264::formats::{RgbSliceU8, YUVBuffer};
    let (w, h) = (128usize, 128usize);
    let rgb: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let x = (i % w) as u8;
            let y = (i / w) as u8;
            [x.wrapping_mul(2), y.wrapping_mul(2), 128u8]
        })
        .collect();
    let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(&rgb, (w, h)));
    let mut enc = Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new()).unwrap();
    let bytes = enc.encode(&yuv).unwrap().to_vec();
    assert!(!bytes.is_empty(), "encoder không ra bitstream");
    let mut r = FrameRenderer::new();
    let jpeg = r.push(&VideoFrame { key: true, data: bytes, ts: 90_000 }, 0.0).expect("decode ra JPEG");
    assert_eq!(&jpeg[0..2], &[0xFF, 0xD8], "thiếu SOI magic");
    assert!(jpeg.len() > 1_000, "JPEG rỗng đáng ngờ");
    // Frame tới ngay sau (< 500 ms) bị throttle nuốt dù decode được.
    assert!(r.push(&VideoFrame { key: false, data: vec![0, 0, 0, 1, 6, 5], ts: 90_003 }, 100.0).is_none());
    // Qua 500 ms mà không có hình mới (SEI lẻ) vẫn là None, không crash.
    assert!(r.push(&VideoFrame { key: false, data: vec![0, 0, 0, 1, 6, 5], ts: 90_006 }, 600.0).is_none());
}
