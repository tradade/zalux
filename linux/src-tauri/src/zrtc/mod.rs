//! The ZRTC media engine — ported wire protocol and call state machine for
//! accepting / placing real calls on Linux. See `.issues/010-call-zrtc-blueprint.md`.
//!
//! Wire builders are a byte-exact port of the Zalos client's
//! `engines/zcall-cpp/engine/src/wire/` (BSD-3-Clause); the golden vectors in
//! `testdata/` pin every layout.
//
// Built ahead of the engine wiring (C1.4-C1.6): the unused warnings go away as
// the state machine, srtp and rtp modules land.
#![allow(dead_code)]

pub mod audio;
pub mod bytes;
pub mod engine;
pub mod group;
pub mod media;
pub mod rtp;
pub mod rtcp;
pub mod srtp;
pub mod video;
pub mod wire;

#[cfg(test)]
mod engine_tests;

#[cfg(test)]
mod group_tests;

#[cfg(test)]
mod rtp_rtcp_tests;

#[cfg(test)]
mod video_tests;

#[cfg(test)]
mod wire_fake_reply_tests;

#[cfg(test)]
mod wire_tests;
