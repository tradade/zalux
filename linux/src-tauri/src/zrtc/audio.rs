//! Audio I/O for the zrtc engine — Opus 16 kHz mono, 20 ms frames, matching
//! the encoder settings of the Zalos audio helper (VoIP mode, 32 kbps,
//! complexity 10, inband FEC, 5% loss expectation).
//!
//! Split so everything but the device streams is unit-testable:
//!
//! * [`JitterBuffer`] — per-call (voice is one stream), holds encoded frames,
//!   adaptive target depth ≥ 5 frames (100 ms), PLC through the decoder.
//! * [`AudioIo`] — the cpal device streams (mic capture ring, speaker render
//!   ring). Built at runtime; without a device the engine reports an audio
//!   fault and the call continues receive-only.

use opus::{Application, Channels, Decoder, Encoder};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

pub const SAMPLE_RATE: u32 = 16_000;
pub const FRAME_MS: usize = 20;
pub const FRAME_SAMPLES: usize = 320; // 16 kHz × 20 ms
pub const BITRATE_BPS: i32 = 32_000;

// ------------------------------------------------------------------ Opus codec

pub struct VoiceEncoder {
    encoder: Encoder,
}

impl VoiceEncoder {
    pub fn new() -> Result<Self, String> {
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip)
            .map_err(|e| format!("opus encoder: {e}"))?;
        encoder.set_bitrate(opus::Bitrate::Bits(BITRATE_BPS)).map_err(|e| e.to_string())?;
        encoder.set_complexity(10).map_err(|e| e.to_string())?;
        encoder.set_inband_fec(true).map_err(|e| e.to_string())?;
        encoder.set_packet_loss_perc(5).map_err(|e| e.to_string())?;
        Ok(Self { encoder })
    }

    /// Encodes one 20 ms frame (320 samples) into `out`; returns the byte length.
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, String> {
        if pcm.len() != FRAME_SAMPLES {
            return Err(format!("frame {} != {FRAME_SAMPLES}", pcm.len()));
        }
        self.encoder.encode(pcm, out).map_err(|e| format!("opus encode: {e}"))
    }
}

pub struct VoiceDecoder {
    decoder: Decoder,
}

impl VoiceDecoder {
    pub fn new() -> Result<Self, String> {
        Ok(Self { decoder: Decoder::new(SAMPLE_RATE, Channels::Mono).map_err(|e| format!("opus decoder: {e}"))? })
    }

    /// Decodes a frame; an **empty** packet is Opus's packet-loss concealment.
    pub fn decode(&mut self, packet: &[u8], pcm: &mut [i16], fec: bool) -> Result<usize, String> {
        self.decoder.decode(packet, pcm, fec).map_err(|e| format!("opus decode: {e}"))
    }
}

// ------------------------------------------------------------------ jitter buffer

/// Holds encoded frames between network arrival and the 20 ms playout clock.
/// The target depth adapts between 3 and 10 frames from the observed arrival
/// spread (simplified from the WSOLA-based jitter of the original); deep
/// buffers are trimmed so the playout catches up after a network stall.
pub struct JitterBuffer {
    queue: VecDeque<Vec<u8>>,
    target: usize,
    last_arrival_ms: Option<f64>,
    spread_sum: f64,
    spread_n: usize,
}

const TARGET_MIN: usize = 3;
const TARGET_MAX: usize = 10;

impl Default for JitterBuffer {
    fn default() -> Self {
        Self { queue: VecDeque::new(), target: TARGET_MIN.max(5), last_arrival_ms: None, spread_sum: 0.0, spread_n: 0 }
    }
}

impl JitterBuffer {
    pub fn push(&mut self, packet: Vec<u8>, now_ms: f64) {
        if let Some(last) = self.last_arrival_ms {
            let gap = (now_ms - last).abs();
            if gap > 1.0 && gap < 1000.0 {
                self.spread_sum += gap;
                self.spread_n += 1;
                if self.spread_n >= 25 {
                    let mean = self.spread_sum / self.spread_n as f64;
                    // A jittery link wants a deeper buffer: 2× the mean gap, clamped.
                    self.target = ((mean * 2.0 / FRAME_MS as f64).ceil() as usize).clamp(TARGET_MIN, TARGET_MAX);
                    self.spread_sum = 0.0;
                    self.spread_n = 0;
                }
            }
        }
        self.last_arrival_ms = Some(now_ms);
        self.queue.push_back(packet);
        if self.queue.len() > self.target + 5 {
            // Catch-up after a stall: drop the oldest half-frame second.
            let excess = self.queue.len() - (self.target + 5);
            for _ in 0..excess {
                self.queue.pop_front();
            }
        }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// `true` once the buffer reached its target depth (start of call ramp-up).
    pub fn ready(&self) -> bool {
        self.queue.len() >= self.target
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        self.queue.pop_front()
    }

    /// Simulates a lost packet: nothing to pop, the decoder runs PLC instead.
    pub fn tick_due(&mut self) -> Option<Vec<u8>> {
        if self.ready() {
            self.queue.pop_front()
        } else {
            None
        }
    }
}

// ------------------------------------------------------------------ device rings

/// A bounded i16 ring shared with a cpal callback (lock hold is a couple of
/// pointer ops; the 20 ms path never blocks on it).
#[derive(Clone, Default)]
pub struct PcmRing {
    inner: Arc<Mutex<VecDeque<i16>>>,
    capacity: usize,
}

impl PcmRing {
    /// `capacity` in samples (2 s of 16 kHz is 32_000; rings use ~300 ms).
    pub fn new(capacity: usize) -> Self {
        Self { inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))), capacity }
    }

    pub fn push(&self, samples: &[i16]) {
        let mut q = self.inner.lock().unwrap();
        for &s in samples {
            if q.len() >= self.capacity {
                q.pop_front(); // drop the oldest: latency matters more than gaps
            }
            q.push_back(s);
        }
    }

    pub fn pop(&self, out: &mut [i16]) -> usize {
        let mut q = self.inner.lock().unwrap();
        let n = out.len().min(q.len());
        for slot in out.iter_mut().take(n) {
            if let Some(s) = q.pop_front() {
                *slot = s;
            }
        }
        n
    }
}

/// Which direction failed, for the engine's audio-fault notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Mic,
    Speaker,
}

/// Live device streams. Held by the host worker; dropping it closes the
/// streams (cpal streams stop when dropped).
pub struct AudioIo {
    _input: cpal::Stream,
    _output: cpal::Stream,
}

/// Mốc ms lần capture mic gần nhất (callback cpal còn sống hay không). Dùng
/// epoch ms như `media::now_ms` để media thread so sánh trực tiếp.
fn capture_now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl AudioIo {
    /// Opens 16 kHz mono streams on the default devices. `mic` receives captured
    /// PCM, `speaker` is drained for playout. Errors name the direction.
    ///
    /// `faults` nhận `Direction` khi stream chết giữa cuộc gọi (rút thiết bị,
    /// thu hồi quyền — cpal báo qua error callback); `mic_activity` được dập
    /// mỗi lần callback capture chạy để watchdog phát hiện callback ngừng
    /// mà không có error (trường hợp hiếm nhưng đã gặp ngoài thực tế).
    pub fn open(mic: PcmRing, speaker: PcmRing, faults: &mpsc::Sender<Direction>, mic_activity: &Arc<AtomicU64>) -> Result<Self, (Direction, String)> {
        let host = cpal::default_host();
        let config = cpal::StreamConfig { channels: 1, sample_rate: cpal::SampleRate(SAMPLE_RATE), buffer_size: cpal::BufferSize::Default };
        let err = |d: Direction| move |e: cpal::BuildStreamError| (d, e.to_string());
        let mic_faults = faults.clone();
        let mic_clock = mic_activity.clone();
        let input = host
            .default_input_device()
            .ok_or((Direction::Mic, "no input device".to_string()))
            .and_then(|device| {
                device.build_input_stream(
                    &config,
                    move |pcm: &[i16], _| {
                        mic_clock.store(capture_now_ms(), Ordering::Relaxed);
                        mic.push(pcm);
                    },
                    move |e| {
                        eprintln!("[olaz] mic stream: {e}");
                        let _ = mic_faults.send(Direction::Mic);
                    },
                    None,
                )
                .map_err(err(Direction::Mic))
            })
            .map_err(|(d, e)| (d, e))?;
        let spk_faults = faults.clone();
        let output = host
            .default_output_device()
            .ok_or((Direction::Speaker, "no output device".to_string()))
            .and_then(|device| {
                device
                    .build_output_stream(&config, move |pcm: &mut [i16], _| {
                        let got = speaker.pop(pcm);
                        for slot in pcm[got..].iter_mut() {
                            *slot = 0; // underrun: silence, the jitter buffer is still ramping
                        }
                    }, move |e| {
                        eprintln!("[olaz] speaker stream: {e}");
                        let _ = spk_faults.send(Direction::Speaker);
                    }, None)
                    .map_err(err(Direction::Speaker))
            })
            .map_err(|(d, e)| (d, e))?;
        input.play().map_err(|e| (Direction::Mic, e.to_string()))?;
        output.play().map_err(|e| (Direction::Speaker, e.to_string()))?;
        Ok(Self { _input: input, _output: output })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_round_trip() {
        let mut enc = VoiceEncoder::new().unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        // A 440 Hz sine survives the VoIP high-pass; a sawtooth would not
        // (the codec is tuned for speech).
        let pcm: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| (3000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / SAMPLE_RATE as f64).sin()) as i16)
            .collect();
        let mut bytes = [0u8; 400];
        let n = enc.encode(&pcm, &mut bytes).unwrap();
        assert!(n > 0 && n <= 400);
        let mut out = [0i16; FRAME_SAMPLES];
        let got = dec.decode(&bytes[..n], &mut out, false).unwrap();
        assert_eq!(got, FRAME_SAMPLES);
        // VoIP at 32 kbps is lossy and phase-shifting; compare energy instead
        // of waveform: the decoded RMS must be in the same ballpark (not
        // silence, not garbage) — what matters for the playout path.
        let rms = |s: &[i16]| (s.iter().map(|v| i64::from(*v) * i64::from(*v)).sum::<i64>() as f64 / s.len() as f64).sqrt();
        let ratio = rms(&out) / rms(&pcm);
        assert!(ratio > 0.3 && ratio < 2.0, "rms ratio {ratio}");
    }

    #[test]
    fn plc_fills_after_loss() {
        let mut dec = VoiceDecoder::new().unwrap();
        let mut out = [0i16; FRAME_SAMPLES];
        dec.decode(&[0u8; 40], &mut out, false).unwrap();
        let got = dec.decode(&[], &mut out, false).unwrap(); // empty packet = PLC
        assert_eq!(got, FRAME_SAMPLES);
    }

    #[test]
    fn jitter_ramp_up_and_catch_up() {
        let mut jb = JitterBuffer::default();
        // Ramp-up: nothing plays until the target depth is reached.
        assert!(jb.tick_due().is_none());
        for i in 0..5 {
            jb.push(vec![i as u8; 10], i as f64 * 20.0);
        }
        assert!(jb.ready());
        assert_eq!(jb.pop().unwrap(), vec![0u8; 10]);
        // Stall: the buffer trims back to target+5 so playout catches up.
        for i in 0..40 {
            jb.push(vec![i as u8; 10], 100.0 + i as f64 * 20.0);
        }
        assert!(jb.len() <= 10 + 5);
    }

    #[test]
    fn pcm_ring_overwrites_oldest() {
        let ring = PcmRing::new(8);
        ring.push(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let mut out = [0i16; 8];
        assert_eq!(ring.pop(&mut out), 8);
        assert_eq!(out, [3, 4, 5, 6, 7, 8, 9, 10]);
    }
}
