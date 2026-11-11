//! Both panners fork like every param node: a fork starts from the values
//! last set through the node's `ParamSet` and shares no cell with the live
//! node, in either direction (`assert_param_fork`).
//!
//! Mutations (run): drop `fork.azimuth.detach()` from either panner's
//! `fork_fresh` -> "a live write reached the fork" for `Azimuth`; likewise
//! the `elevation` detach, VBAP's `spread` and `width`, and HRTF's `blend`
//! (`Wet`).

use tutti_graph::ParamNode;
use tutti_graph::contract::{SAMPLE_RATE, assert_param_fork};
use tutti_nodes::{HRTF_BINAURAL_PARAMS, HrtfBinauralNode, VBAP_PANNER_PARAMS, VbapPannerNode};
use tutti_types::SampleRate;

fn hrtf() -> HrtfBinauralNode {
    let rate = SAMPLE_RATE.get() as u32;
    let bytes = synthetic_hrir_sphere(rate, 64);
    HrtfBinauralNode::new(&bytes, SampleRate::from(rate)).expect("the sphere parses")
}

#[test]
fn vbap_forks_from_its_params() {
    assert_param_fork(VbapPannerNode::atmos_7_1_4());
}

#[test]
fn hrtf_forks_from_its_params() {
    assert_param_fork(hrtf());
}

/// Every param a panner lets a lane drive is one its `ParamSet` addresses,
/// so a host reaches the cell a lane rides on.
///
/// Mutation (run): drop `UnitParam::StereoWidth` from `param_set` -> the
/// VBAP row fails.
#[test]
fn a_panner_declares_every_param_it_reads() {
    let vbap = VbapPannerNode::stereo().param_set();
    for p in VBAP_PANNER_PARAMS {
        assert!(vbap.get(p).is_some(), "VBAP: {p:?} is not in its ParamSet");
    }
    let hrtf = hrtf().param_set();
    for p in HRTF_BINAURAL_PARAMS {
        assert!(hrtf.get(p).is_some(), "HRTF: {p:?} is not in its ParamSet");
    }
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
