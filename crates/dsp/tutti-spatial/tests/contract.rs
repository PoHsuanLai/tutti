//! The graph's sample-accuracy contract for the binaural
//! renderer, a graph node, on the audio-impulse path: an impulse at
//! frame `F` leaves at exactly `F + arrival + FRAME_LEN - 1` on every path
//! (direct, behind PDC, across a recompile, across ragged blocks — see
//! `tutti_graph::contract`).
//!
//! A node that declared no latency while every sample left a frame late would
//! leave PDC blind to the delay, and binaural tracks would arrive late against
//! the mix. The row pins
//! the declared figure (511 at the panner's 4 × 128 frame) *and* that the
//! output leaves on it, wherever in the HRTF frame and in the graph's block
//! the impulse falls.
//!
//! Mutations (run): declare `Latency::ZERO` in `shape` → every
//! path fails (declared 0, output 511 late). Set `LATENCY` to `FRAME_LEN` →
//! every path fails (declared one frame late).

use tutti_core::Samples;
use tutti_graph::contract::{Detect, Excite, Row, SAMPLE_RATE};
use tutti_graph::contract_tests;
use tutti_spatial::HrtfBinauralNode;

/// The panner's frame (`INTERPOLATION_STEPS * BLOCK_LEN` in `panner.rs`),
/// less the one sample its push-then-drain saves.
const LATENCY: usize = 4 * 128 - 1;

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

fn node() -> HrtfBinauralNode {
    let rate = SAMPLE_RATE.get() as u32;
    let bytes = synthetic_hrir_sphere(rate, 64);
    // Born at the contract's rate; the graph prepares it at that rate too.
    let node = HrtfBinauralNode::new(&bytes, SAMPLE_RATE).expect("the sphere parses");
    node.set_position(90.0, 0.0);
    node
}

/// The impulse on the left input; each output is a fold of both inputs, so
/// either ear hears it on the same frame. Checked on each ear.
fn hrtf_row(ear: u16) -> Row {
    Row::new(
        &format!("HrtfBinauralNode (output {ear})"),
        || Box::new(node()),
        Excite::Impulse {
            port: 0,
            amplitude: 0.5,
        },
        Detect::Threshold(1e-4),
    )
    .output(ear)
    .expect_latency(Samples(LATENCY))
}

contract_tests!(audio hrtf_left => hrtf_row(0));
contract_tests!(audio hrtf_right => hrtf_row(1));
