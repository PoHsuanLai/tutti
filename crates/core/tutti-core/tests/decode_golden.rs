//! Golden decode digests for the committed fixtures in `assets/audio/`.
//!
//! Pins what the engine's decode path produces, bit for bit, so that moving
//! the decoder out of the fundsp fork (design doc 013, Phase 0) can be shown to
//! change nothing. Every sample of every channel is folded into an FNV-1a
//! digest over its `f32` bit pattern — a tolerance would let a changed
//! conversion or a dropped packet through.
#![cfg(all(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]

use std::path::PathBuf;

use tutti_core::{AudioIn, FileIn, Wave};

fn asset(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../assets/audio")
        .join(name)
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn f32(&mut self, v: f32) {
        self.u64(u64::from(v.to_bits()));
    }
}

fn wave_digest(w: &Wave) -> u64 {
    let mut h = Fnv::new();
    h.u64(w.channels() as u64);
    h.u64(w.len() as u64);
    h.u64(w.sample_rate().get().to_bits());
    for c in 0..w.channels() {
        for i in 0..w.len() {
            h.f32(w.at(c, i));
        }
    }
    h.0
}

/// Read the whole file through `FileIn` in odd-sized chunks, so reads cross
/// packet boundaries mid-packet, and digest the interleaved stream.
fn stream_digest(name: &str) -> (u64, usize) {
    let mut dec = FileIn::open(asset(name), None).expect("open");
    let ch = dec.layout().count() as usize;
    let mut buf = vec![0.0f32; 333 * ch];
    let mut h = Fnv::new();
    let mut frames = 0usize;
    loop {
        let n = dec.poll_into(&mut buf).0;
        if n == 0 {
            break;
        }
        for &s in &buf[..n * ch] {
            h.f32(s);
        }
        frames += n;
    }
    (h.0, frames)
}

/// `(file, load digest, streamed digest)`.
///
/// Recorded from the fork's decoder (`fundsp::read` / `fundsp::stream`) before
/// the move; the rehomed decoder must reproduce them exactly.
///
/// Mutation: shortening every decoded packet by one frame in the shared
/// packet-decode helper changes every row that has audio (checked).
const GOLDEN: &[(&str, u64, u64)] = &[
    ("stereo_s16.wav", 0x3a5e069030cee9fe, 0x5983063d609be7ed),
    ("stereo_s24.wav", 0x9e907ce1c8607f10, 0xb9f775ef850a7f23),
    ("mono_48k.wav", 0xcdad4db62795bafc, 0x48ca681f1e6cefd5),
    ("surround_51.wav", 0xe130b7339862140b, 0xdf0812ca019020c5),
    // Same digests as the 24-bit WAV: ffmpeg writes FLAC at 24 bits from the
    // same generator, and a lossless decode must land on the same samples.
    ("stereo.flac", 0x9e907ce1c8607f10, 0xb9f775ef850a7f23),
    ("stereo.mp3", 0x83e5be1d21986f6e, 0x03180728f1ec4892),
    // The streamed digest is the EMPTY digest: `FileIn` reads zero frames from
    // this file. Recorded as found, so the move is shown to change nothing.
    ("stereo.ogg", 0x40a6b908a599a73c, 0xcbf29ce484222325),
];

#[test]
fn decoded_samples_match_the_golden_digests() {
    let mut failures = Vec::new();
    for &(name, want_load, want_stream) in GOLDEN {
        let w = Wave::load(asset(name)).expect("load");
        let got_load = wave_digest(&w);
        let (got_stream, frames) = stream_digest(name);
        if got_load != want_load || got_stream != want_stream {
            failures.push(format!(
                "(\"{name}\", {got_load:#018x}, {got_stream:#018x}), // {} ch, {} frames, streamed {frames}",
                w.channels(),
                w.len()
            ));
        }
    }
    assert!(failures.is_empty(), "digest mismatch:\n{}", failures.join("\n"));
}

/// After a seek, `FileIn` produces exactly the frames a whole-file load has at
/// that position, for the lossless containers. (A lossy decoder restarted by a
/// seek has no overlap state for its first packet, so sample identity is not
/// its contract.) The probe agrees with the load on width and rate for all.
///
/// Mutation: make `FileIn::seek` discard one extra preroll frame, and the slice
/// comparison below fails at its first frame (checked).
#[test]
fn seek_lands_on_the_whole_file_load_frames() {
    for &(name, _, _) in GOLDEN {
        let w = Wave::load(asset(name)).expect("load");
        let meta = Wave::probe_metadata(asset(name)).expect("probe");
        assert_eq!(meta.channels, w.channels(), "{name}: probed width");
        assert_eq!(
            meta.sample_rate as f64,
            w.sample_rate().get(),
            "{name}: probed rate"
        );

        if name.ends_with(".mp3") || name.ends_with(".ogg") {
            continue;
        }
        let ch = w.channels();
        let start = w.len() / 3;
        let mut dec = FileIn::open(asset(name), None).expect("open");
        assert!(dec.seekable(), "{name}: a lossless fixture must be seekable");
        dec.seek(start as u64).expect("seek");
        let want = 257.min(w.len() - start);
        let mut out = vec![0.0f32; want * ch];
        assert_eq!(dec.poll_into(&mut out).0, want, "{name}: seek read count");
        for f in 0..want {
            for c in 0..ch {
                assert_eq!(
                    out[f * ch + c].to_bits(),
                    w.at(c, start + f).to_bits(),
                    "{name}: frame {} channel {c} after seek",
                    start + f
                );
            }
        }
    }
}

