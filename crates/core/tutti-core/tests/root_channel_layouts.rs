//! The graph root's channel count must not panic the audio callback, AND a
//! surround root must reach the output correctly — either straight through to a
//! matching-width device, or folded to a narrower one via the ITU/Dolby matrix.
//!
//! `TuttiPlugin.outputs` is a public field, so a host can build a mono or wider
//! root. `Net::process` iterates `output.channels()` and indexes its own
//! `output_edge` table by that channel — so a scratch buffer wider than the net
//! indexes past the end and panics *in release*, inside the CPAL callback. These
//! run in both profiles on purpose: the failure mode was release-only.

use tutti_core::dsp::{sine_hz, Net};
use tutti_core::{Engine, MotionFsm, TransportSettings};

/// Render one block of a root with `outputs` channels into a `target`-wide
/// interleaved buffer. `wire` connects the source to root outputs (its `NodeId`
/// output 0 is fed to whichever channels `wire` picks).
fn render_root_to(outputs: usize, target: usize, wire: &[usize]) -> Vec<f32> {
    let mut net = Net::new(0, outputs);
    let id = net.push(Box::new(sine_hz::<f32>(440.0)));
    for &ch in wire {
        net.connect_output(id, 0, ch);
    }
    let engine = Engine::new(MotionFsm::new(TransportSettings::new()), net.backend());
    let frames = 256;
    let mut out = vec![0.0f32; frames * target];
    engine.process(&mut out, frames, target);
    out
}

/// Sum of squares of channel `c` in a `width`-interleaved buffer.
fn channel_energy(buf: &[f32], c: usize, width: usize) -> f32 {
    buf.chunks_exact(width).map(|f| f[c] * f[c]).sum()
}

#[test]
fn mono_root_duplicates_into_stereo() {
    // Mono root → stereo device: channel 0 duplicated into both sides.
    let out = render_root_to(1, 2, &[0]);
    for frame in out.chunks_exact(2) {
        assert_eq!(frame[0], frame[1], "mono root must duplicate into L/R");
    }
    assert!(out.iter().any(|s| *s != 0.0), "sine should produce signal");
}

#[test]
fn stereo_root_passes_through_to_stereo() {
    let out = render_root_to(2, 2, &[0, 1]);
    assert!(out.iter().any(|s| *s != 0.0));
}

#[test]
fn surround_root_reaches_matching_width_device() {
    // 5.1 root, source wired ONLY to the SL channel (idx 4). At a 6-wide target
    // the energy stays in channel 4 — straight through, no fold.
    let out = render_root_to(6, 6, &[4]);
    let e: Vec<f32> = (0..6).map(|c| channel_energy(&out, c, 6)).collect();
    assert!(e[4] > 0.0, "SL source must reach output channel 4 ({e:?})");
    for (c, &en) in e.iter().enumerate() {
        if c != 4 {
            assert_eq!(en, 0.0, "only channel 4 should carry energy ({e:?})");
        }
    }
}

#[test]
fn surround_root_folds_to_stereo_device() {
    // Same 5.1 root with the source in SL (idx 4). At a 2-wide target the ITU
    // matrix folds SL into Lo at −3 dB (Lo = FL + .707·C + .707·SL) — so the
    // LEFT channel carries it and the right is (near) silent.
    let out = render_root_to(6, 2, &[4]);
    let el = channel_energy(&out, 0, 2);
    let er = channel_energy(&out, 1, 2);
    assert!(el > 0.0, "SL must fold into the left downmix channel");
    assert!(
        el > er * 4.0,
        "SL folds to the left only (el={el}, er={er})"
    );
}

#[test]
fn center_root_folds_symmetrically_to_stereo() {
    // Source in the CENTER channel (idx 2 in 5.1) → both stereo channels at −3dB.
    let out = render_root_to(6, 2, &[2]);
    let el = channel_energy(&out, 0, 2);
    let er = channel_energy(&out, 1, 2);
    assert!(el > 0.0 && er > 0.0, "center must reach both channels");
    let ratio = el / er;
    assert!(
        (0.5..2.0).contains(&ratio),
        "center fold must be ~symmetric"
    );
}

#[test]
fn over_wide_root_clamps_without_panicking() {
    // A root wider than MAX_ROOT_CHANNELS (8) renders its first 8 channels
    // rather than indexing past the scratch. Folded to stereo here.
    let wide = 12;
    let out = render_root_to(wide, 2, &(0..wide).collect::<Vec<_>>());
    assert!(
        out.iter().any(|s| *s != 0.0),
        "over-wide root produced silence"
    );
}
