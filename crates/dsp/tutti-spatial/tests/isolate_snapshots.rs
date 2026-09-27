//! Both panners fork to a **snapshot** of their placement: once a fork is
//! taken from the source their insert hands the editor, no live move of the
//! position, spread, width or blend reaches it (doc 013, gap 6's audit).
//!
//! The panners are native nodes whose controls are typed cells no
//! `UnitParam` addresses, so their fork is not `tutti_graph::param_parts`'
//! and `assert_param_fork` cannot check it. [`check_fork`] runs the steps
//! the graph contract's isolate row runs, on the panners' own fork:
//!
//! 1. take two forks, and render the first;
//! 2. move one control through the controls the insert handed back;
//! 3. render the second fork: it must match the first, sample for sample;
//! 4. render a fork taken *after* the move: it must differ, or the row
//!    cannot tell a severed cell from a shared one.
//!
//! Mutations (run): drop `controls.detach()` from either panner's
//! `fork_fresh` → its azimuth control fails ("a live move reached the
//! fork"); delete a `width`/`spread`/`blend` detach in its controls'
//! `detach` → that control fails; drop `SpatialTarget::detach`'s elevation
//! line → both elevation controls fail.

use tutti_core::{Mix, SampleRate, Spread, StereoWidth};
use tutti_graph::contract::{drive, SAMPLE_RATE};
use tutti_graph::{ForkMode, IntoNode, Node, NodeParts, Prepare};
use tutti_spatial::{HrtfBinauralControls, HrtfBinauralNode, VbapPannerControls, VbapPannerNode};
use tutti_types::Samples;

/// Frames per render: long enough for the 50 ms de-zipper to settle, and for
/// several loud/quiet stimulus cycles.
const FRAMES: usize = 16_384;
const BLOCK: usize = 64;

/// A control a row moves, through the controls the insert handed back.
type Move<C> = (&'static str, fn(&C));

/// The four steps in the module docs, for each control in `moves`.
fn check_fork<N: IntoNode>(name: &str, make: impl Fn() -> N, moves: &[Move<N::Controls>]) {
    assert!(!moves.is_empty(), "{name}: a row with no controls");
    for (control, write) in moves {
        let NodeParts { controls, fork, .. } = make().into_parts();
        let fork = fork.unwrap_or_else(|| panic!("{name}: the node does not fork"));
        let take = || fork.fork(ForkMode::Live).expect("the fork succeeds").node;
        let (first, second) = (take(), take());
        let before = render(first);

        write(&controls);

        let after = render(second);
        if let Some((ch, frame)) = first_difference(&before, &after) {
            panic!(
                "{name} / {control}: a live move reached the fork (channel {ch}, frame {frame}: \
                 {} before, {} after) — its fork left the control's cell shared",
                before[ch][frame], after[ch][frame]
            );
        }
        let moved = render(take());
        assert!(
            first_difference(&before, &moved).is_some(),
            "{name} / {control}: moving it did not change a fresh fork's output, so this row \
             cannot tell a severed cell from a shared one — move it further"
        );
    }
}

/// `node` prepared (what the fork's graph does), reset, and rendered for
/// [`FRAMES`] of the stimulus in [`BLOCK`]-frame blocks.
fn render(mut node: Box<dyn Node>) -> Vec<Vec<f32>> {
    node.prepare(&Prepare::new(SAMPLE_RATE, Samples(BLOCK)));
    node.reset();
    let ins = usize::from(node.shape().audio_in.count());
    let mut out = vec![Vec::with_capacity(FRAMES); usize::from(node.shape().audio_out.count())];
    for start in (0..FRAMES).step_by(BLOCK) {
        let input: Vec<Vec<f32>> = (0..ins)
            .map(|c| (start..start + BLOCK).map(|f| stimulus(c, f)).collect())
            .collect();
        let refs: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
        for (o, block) in out
            .iter_mut()
            .zip(drive(&mut *node, SAMPLE_RATE, &refs, &[]))
        {
            o.extend(block);
        }
    }
    out
}

/// A tone per channel under a loud/quiet envelope, plus fixed noise: L and
/// R differ, so the width is heard.
fn stimulus(ch: usize, frame: usize) -> f32 {
    let envelope = if (frame / 1024).is_multiple_of(2) {
        0.9
    } else {
        0.05
    };
    let hz = 110.0 * (ch + 2) as f32;
    let phase = frame as f32 * hz / SAMPLE_RATE.get() as f32;
    let tone = (core::f32::consts::TAU * phase).sin();
    let seed = (frame as u32)
        .wrapping_mul(1_664_525)
        .wrapping_add(1_013_904_223 ^ (ch as u32).wrapping_mul(0x9E37_79B9));
    let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
    envelope * (0.8 * tone + 0.2 * noise)
}

fn first_difference(a: &[Vec<f32>], b: &[Vec<f32>]) -> Option<(usize, usize)> {
    a.iter().zip(b).enumerate().find_map(|(ch, (x, y))| {
        x.iter()
            .zip(y)
            .position(|(p, q)| p.to_bits() != q.to_bits())
            .map(|frame| (ch, frame))
    })
}

/// An Atmos bed, so a height move is audible (a flat ring would ignore it).
#[test]
fn vbap() {
    check_fork(
        "VbapPannerNode (7.1.4)",
        || {
            let node = VbapPannerNode::atmos_7_1_4().expect("the Atmos preset builds");
            node.set_position(30.0, 0.0);
            node.set_spread(Spread::new_clamped(0.2));
            node
        },
        &[
            ("azimuth", |c: &VbapPannerControls| {
                c.set_position(-110.0, 0.0)
            }),
            ("elevation", |c| c.set_position(30.0, 45.0)),
            ("spread", |c| c.set_spread(Spread::new_clamped(0.8))),
            ("width", |c| c.set_width(StereoWidth::new_clamped(1.8))),
        ],
    );
}

#[test]
fn hrtf() {
    check_fork(
        "HrtfBinauralNode",
        || {
            let rate = SAMPLE_RATE.get() as u32;
            let bytes = synthetic_hrir_sphere(rate, 64);
            let node =
                HrtfBinauralNode::new(&bytes, SampleRate::from(rate)).expect("the sphere parses");
            node.set_position(90.0, 0.0);
            node
        },
        &[
            ("azimuth", |c: &HrtfBinauralControls| {
                c.set_position(-150.0, 0.0)
            }),
            ("elevation", |c| c.set_position(90.0, 60.0)),
            ("blend", |c| c.set_blend(Mix::new_clamped(0.3))),
        ],
    );
}

/// A minimal, format-valid HRIR sphere: a tetrahedron of 4 vertices with
/// unit-delta HRIRs (left an identity delta, right scaled per vertex), at
/// the contract's rate so nothing is resampled. The same construction as
/// the node's unit tests in `src/hrtf/node.rs`, which are private to it.
fn synthetic_hrir_sphere(sample_rate: u32, ir_len: usize) -> Vec<u8> {
    fn push_u32(b: &mut Vec<u8>, v: u32) {
        b.extend_from_slice(&v.to_le_bytes());
    }
    fn push_f32(b: &mut Vec<u8>, v: f32) {
        b.extend_from_slice(&v.to_le_bytes());
    }
    let verts: [[f32; 3]; 4] = [
        [1.0, 1.0, 1.0],
        [-1.0, -1.0, 1.0],
        [-1.0, 1.0, -1.0],
        [1.0, -1.0, -1.0],
    ];
    let faces: [[u32; 3]; 4] = [[0, 1, 2], [0, 3, 1], [0, 2, 3], [1, 3, 2]];
    let mut b = Vec::new();
    b.extend_from_slice(b"HRIR");
    push_u32(&mut b, sample_rate);
    push_u32(&mut b, ir_len as u32);
    push_u32(&mut b, verts.len() as u32);
    push_u32(&mut b, (faces.len() * 3) as u32);
    for f in faces {
        for idx in f {
            push_u32(&mut b, idx);
        }
    }
    for (vi, v) in verts.iter().enumerate() {
        for x in v {
            push_f32(&mut b, *x);
        }
        for i in 0..ir_len {
            push_f32(&mut b, if i == 0 { 1.0 } else { 0.0 });
        }
        for i in 0..ir_len {
            push_f32(&mut b, if i == 0 { 0.5 + 0.1 * vi as f32 } else { 0.0 });
        }
    }
    b
}
