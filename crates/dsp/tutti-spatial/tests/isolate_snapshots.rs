//! Both panners fork to a **snapshot** of their placement: after `clone` +
//! `isolate`, no live move of the position, spread, width or blend cells
//! reaches the fork (doc 013, gap 6's audit). See
//! `tutti_graph::contract::IsolateRow` for the four steps each control runs.
//!
//! Mutations (run): delete `self.target.detach()` from either panner's
//! `isolate` → its azimuth and elevation controls fail ("a live move
//! reached the fork"); delete a `width`/`spread` detach → that control
//! fails; drop `SpatialTarget::detach`'s elevation line → both elevation
//! controls fail.

use tutti_core::{AudioUnit, Mix, Spread, StereoWidth};
use tutti_graph::contract::{IsolateRow, SAMPLE_RATE};
use tutti_spatial::{HrtfBinauralNode, VbapPannerNode};

/// An Atmos bed, so a height move is audible (a flat ring would ignore it).
#[test]
fn vbap() {
    IsolateRow::new("VbapPannerNode (7.1.4)", || {
        let node = VbapPannerNode::atmos_7_1_4().expect("the Atmos preset builds");
        node.set_position(30.0, 0.0);
        node.set_spread(Spread::new_clamped(0.2));
        node
    })
    .control("azimuth", |n| n.set_position(-110.0, 0.0))
    .control("elevation", |n| n.set_position(30.0, 45.0))
    .control("spread", |n| n.set_spread(Spread::new_clamped(0.8)))
    .control("width", |n| n.set_width(StereoWidth::new_clamped(1.8)))
    .check();
}

#[test]
fn hrtf() {
    IsolateRow::new("HrtfBinauralNode", || {
        let rate = SAMPLE_RATE.get() as u32;
        let bytes = synthetic_hrir_sphere(rate, 64);
        let mut node = HrtfBinauralNode::new(&bytes, SAMPLE_RATE).expect("the sphere parses");
        node.set_position(90.0, 0.0);
        node.set_sample_rate(SAMPLE_RATE);
        node
    })
    .control("azimuth", |n| n.set_position(-150.0, 0.0))
    .control("elevation", |n| n.set_position(90.0, 60.0))
    .control("blend", |n| n.set_blend(Mix::new_clamped(0.3)))
    .check();
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
