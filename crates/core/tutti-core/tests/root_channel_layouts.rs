//! The graph root's channel count must not be able to panic the audio callback.
//!
//! `TuttiPlugin.outputs` is a public field, so a host can build a mono or wider
//! root. `Net::process` iterates `output.channels()` and indexes its own
//! `output_edge` table by that channel — so a scratch buffer wider than the net
//! indexes past the end and panics *in release*, inside the CPAL callback.
//!
//! These run in both profiles on purpose: the failure mode was release-only,
//! because the debug build tripped an assert first and hid it.

use tutti_core::dsp::{sine_hz, Net};
use tutti_core::{Engine, MotionFsm, TransportSettings};

/// Render one block through a root with `outputs` channels.
fn render_root(outputs: usize) -> Vec<f32> {
    let mut net = Net::new(0, outputs);
    let id = net.push(Box::new(sine_hz::<f32>(440.0)));
    // Feed every root output from the same source, so each has a real edge.
    for ch in 0..outputs {
        net.connect_output(id, 0, ch);
    }

    let engine = Engine::new(MotionFsm::new(TransportSettings::new()), net.backend());
    let frames = 256;
    let mut out = vec![0.0f32; frames * 2];
    engine.process(&mut out, frames);
    out
}

#[test]
fn mono_root_renders_instead_of_panicking() {
    let out = render_root(1);
    // Mono duplicates channel 0 into both sides.
    for frame in out.chunks_exact(2) {
        assert_eq!(
            frame[0], frame[1],
            "a mono root must duplicate its channel into L/R"
        );
    }
    assert!(
        out.iter().any(|s| *s != 0.0),
        "the sine should have produced signal"
    );
}

#[test]
fn stereo_root_renders() {
    let out = render_root(2);
    assert!(out.iter().any(|s| *s != 0.0));
}

/// A root wider than stereo renders its first two channels rather than panicking
/// or silently indexing out of bounds. Channels beyond the second are dropped —
/// the device stream is stereo — but that is a documented narrowing, not a crash.
#[test]
fn wide_roots_render_their_first_two_channels() {
    for outputs in [3, 4, 6, 8] {
        let out = render_root(outputs);
        assert!(
            out.iter().any(|s| *s != 0.0),
            "{outputs}-channel root produced silence"
        );
    }
}
