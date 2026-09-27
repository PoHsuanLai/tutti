//! Regression gate for RT-safety: the panner nodes must not allocate on the
//! process path.
//!
//! This gate moved here with the panners. It was `tutti-nodes`' until that
//! crate lost its `spatial` feature, at which point the `#[cfg(feature =
//! "spatial")]` on each test became permanently false and both stopped
//! running — silently, since a test that is never compiled cannot fail.

use assert_no_alloc::AllocDisabler;
use tutti_core::SampleRate;
use tutti_graph::contract::BlockRig;
use tutti_graph::{IntoNode, Node};
use tutti_spatial::VbapPannerNode;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// `node` alone in a graph (`BlockRig`) at 48 kHz in 64-frame blocks, fed a
/// DC pair: `warm` blocks, then 1 000 under `assert_no_alloc`.
///
/// The inputs carry signal, and the gate checks it came out, so a gate over a node that renders nothing cannot pass
/// having walked no DSP. Mutation (run): `fill(0.0)` → "rendered silence".
fn gate<N: IntoNode>(node: N, warm: usize) {
    let (mut rig, _controls) = BlockRig::new(node, SampleRate(48_000.0), 64);
    for c in rig.inputs_mut() {
        c.fill(0.5);
    }
    for _ in 0..warm {
        rig.block();
    }
    assert!(
        rig.output(0)
            .iter()
            .chain(rig.output(1))
            .any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate would walk no DSP"
    );
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            rig.block();
        }
    });
}

#[test]
fn vbap_panner_stereo_process_is_allocation_free() {
    let node = VbapPannerNode::stereo().expect("stereo preset");
    node.set_position(30.0, 0.0);
    // One warm-up block.
    gate(node, 1);
}

/// The rear-arc **fold** branch is allocation-free.
///
/// `vbap_panner_stereo_process_is_allocation_free` above pans to 30 degrees,
/// which is inside the speaker pair and takes neither branch `solve_gains`
/// added. A bearing past the lateral axis on a front-only layout takes the
/// fold: the azimuth is mirrored before the solve.
///
/// The fold itself is scalar arithmetic on an `Azimuth` and cannot allocate;
/// what this pins is that it does not push the solve onto a different path
/// inside `vbap` that does.
///
/// Mutation: have `solve_gains` build its own `Vec` for the gains instead of
/// writing into the caller's scratch -> fails here and in both siblings.
#[test]
fn vbap_folded_rear_arc_is_allocation_free() {
    let mut node = VbapPannerNode::stereo().expect("stereo preset");
    // 200 degrees wraps to -160, well past the lateral axis: the fold fires and
    // mirrors it to -20. Plain VBAP would leave this bearing silent.
    node.set_position(200.0, 0.0);
    // `reset` seats the de-zipper on the folded bearing, so the assertion
    // window measures the steady state rather than the ramp into it.
    Node::reset(&mut node);
    gate(node, 1);
}

/// The **elevation-retreat** branch is allocation-free.
///
/// The other branch `solve_gains` added, and the only one that re-enters
/// `vbap`'s solver more than once per sample. A 2D ring has no geometry at
/// exactly the poles — the horizontal projection of the direction vector is the
/// zero vector — so the first solve returns all zeros and the retreat halves the
/// elevation and solves again.
///
/// Quad rather than stereo because the pole is where a 2D ring degenerates, and
/// quad is the smallest such layout. The loop is bounded by
/// `MAX_ELEVATION_RETREATS`, so what this pins is that *iterating* it allocates
/// nothing: the same caller-owned scratch is rewritten in place each round, and
/// nothing per-round reaches the heap.
///
/// Mutation: have the retreat allocate a fresh gains buffer per round -> fails
/// here and nowhere else, since this is the only test that reaches the loop
/// body twice.
#[test]
fn vbap_elevation_retreat_is_allocation_free() {
    let mut node = VbapPannerNode::quad().expect("quad preset");
    // Straight up: a 2D ring cannot solve this, so the retreat runs.
    node.set_position(0.0, 90.0);
    Node::reset(&mut node);
    gate(node, 1);
}

/// Minimal format-valid HRIR sphere (a tetrahedron with delta impulse
/// responses) — enough for the crate to parse and convolve without a dataset.
#[cfg(feature = "hrtf")]
fn synthetic_hrir_sphere(sample_rate: u32, ir_len: usize) -> Vec<u8> {
    let verts: [[f32; 3]; 4] = [
        [1.0, 1.0, 1.0],
        [-1.0, -1.0, 1.0],
        [-1.0, 1.0, -1.0],
        [1.0, -1.0, -1.0],
    ];
    let faces: [[u32; 3]; 4] = [[0, 1, 2], [0, 3, 1], [0, 2, 3], [1, 3, 2]];
    let mut b = Vec::new();
    b.extend_from_slice(b"HRIR");
    b.extend_from_slice(&sample_rate.to_le_bytes());
    b.extend_from_slice(&(ir_len as u32).to_le_bytes());
    b.extend_from_slice(&(verts.len() as u32).to_le_bytes());
    b.extend_from_slice(&((faces.len() * 3) as u32).to_le_bytes());
    for f in faces {
        for idx in f {
            b.extend_from_slice(&idx.to_le_bytes());
        }
    }
    for v in verts {
        for comp in v {
            b.extend_from_slice(&comp.to_le_bytes());
        }
        for _ in 0..2 {
            for i in 0..ir_len {
                b.extend_from_slice(&(if i == 0 { 1.0f32 } else { 0.0 }).to_le_bytes());
            }
        }
    }
    b
}

#[cfg(feature = "hrtf")]
#[test]
fn hrtf_binaural_process_is_allocation_free() {
    use tutti_spatial::HrtfBinauralNode;

    let bytes = synthetic_hrir_sphere(48_000, 64);
    let node = HrtfBinauralNode::new(&bytes, 48_000.0).expect("synthetic sphere parses");
    node.set_position(30.0, 0.0);
    // Warm up past a full HRTF frame so the FFT-convolution path fires and any
    // lazy buffer growth has already happened before the assertion window.
    gate(node, 64);
}
