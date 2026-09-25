//! The graph root's channel count must not panic the audio callback, AND a
//! surround root must reach the output correctly — either straight through to a
//! matching-width device, or folded to a narrower one via the ITU/Dolby matrix.
//!
//! `TuttiPlugin.outputs` is a public field, so a host can build a mono or wider
//! root. A root wider than the engine's fold scratch once indexed past its end
//! and panicked *in release*, inside the CPAL callback (a `Net` iterated its
//! own output table by the scratch's width). The engine now renders only the
//! native graph (doc 013 Phase 3 PR 15) and refuses such a root on the control
//! thread instead; these still run in both profiles on purpose.

mod support;

use support::Sine;
use tutti_core::{
    ChannelLayout, Engine, GraphEngineError, Hz, InterleavedMut, SampleRate, Samples,
};
use tutti_core::{Transport, MAX_ROOT_CHANNELS};
use tutti_graph::{CommitError, Editor, Legacy, Prepare};
use tutti_types::graph::{OutPort, Source};
use tutti_types::NodeKey;

/// A graph whose root has `outputs` channels, with a sine at key 1 feeding
/// the channels `wire` picks (the rest explicitly silent).
fn root(outputs: usize, wire: &[usize]) -> (Editor, tutti_graph::Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(256)));
    ed.insert(NodeKey(1), "sine", Legacy::new(Sine::new(Hz(440.0))));
    ed.spec_mut().topology.outputs = (0..outputs)
        .map(|ch| {
            if wire.contains(&ch) {
                Source::Node(OutPort {
                    node: NodeKey(1),
                    port: 0,
                })
            } else {
                Source::Zero
            }
        })
        .collect();
    ed.commit().expect("commits");
    (ed, exec)
}

/// Render one block of a root with `outputs` channels into a `target`-wide
/// interleaved buffer. `wire` feeds the source to the root outputs it picks.
fn render_root_to(outputs: usize, target: usize, wire: &[usize]) -> Vec<f32> {
    let (mut ed, exec) = root(outputs, wire);
    let engine = Engine::new(&Transport::new(48_000.0), &mut ed, exec).expect("within the limits");
    let frames = 256;
    let mut out = vec![0.0f32; frames * target];
    // `target` stays a plain count in the assertions below — they read as
    // widths there. It becomes a layout at exactly this boundary, welded to the
    // buffer, so the engine's frame count is `out.len() / target` by
    // construction rather than a second argument that could disagree with it.
    engine.process(&mut InterleavedMut::new(
        &mut out,
        ChannelLayout::from(target),
    ));
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
    for frame in out.as_chunks::<2>().0 {
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

/// A root wider than `MAX_ROOT_CHANNELS` (8) never reaches the callback:
/// the engine refuses it at construction, naming the widths.
///
/// Until doc 013 PR 15 this rendered a 12-wide `Net` root and checked it
/// clamped to 8 channels without panicking. A native graph cannot be handed
/// to the engine that wide, so the property is the refusal (the refusal of
/// a later widening commit is `engine_graph`'s
/// `a_graph_engine_refuses_more_outputs_than_it_folds`).
///
/// Mutation (run): `max_global_outputs: usize::MAX` in
/// `Engine::with_capacity` → the engine is built → fails.
#[test]
fn over_wide_root_is_refused_before_it_can_render() {
    let wide = 12;
    let (mut ed, exec) = root(wide, &(0..wide).collect::<Vec<_>>());
    let refused = Engine::new(&Transport::new(48_000.0), &mut ed, exec).err();
    assert_eq!(
        refused,
        Some(GraphEngineError::Limits(CommitError::TooManyOutputs {
            outputs: wide,
            limit: MAX_ROOT_CHANNELS,
        }))
    );
}
