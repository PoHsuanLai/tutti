//! The position de-zipper of `VbapPannerNode` renders what it always did, bit
//! for bit (doc 024 step 0b, consolidation 2): a digest (FNV-1a over the bits
//! of every output sample) of stereo and 5.1 renders of seeded noise whose
//! position jumps across the seam halfway, recorded before `ExponentialSmoother`
//! was built over `tutti_dsp::Smoother`. The render reaches libm (`sin`, `cos`,
//! `atan2` in the gain solve), so the digests go through `assert_golden`.
//!
//! Mutation (run): `Smoother::gap_closed` with `powi(steps + 1)` changes the
//! position's last bits and fails both cases (the panner advances a block in
//! one closed-form move; the HRTF panner's single step is pinned in
//! `hrtf_render.rs`).

use tutti_graph::contract::{drive, prepared};
use tutti_nodes::VbapPannerNode;
use tutti_nodes::testing::assert_golden;
use tutti_types::{Azimuth, Elevation, SampleRate};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;
const BLOCKS: usize = 40;

fn noise(seed: u32) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..BLOCK * BLOCKS)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        })
        .collect()
}

fn digest_of(mut node: VbapPannerNode) -> u64 {
    let controls = node.controls();
    controls.set_position(Azimuth(170.0), Elevation(10.0));
    tutti_graph::Node::reset(&mut node);
    let mut node = prepared(node, RATE, BLOCK);
    let planes = [noise(3), noise(4)];
    let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
    for b in 0..BLOCKS {
        if b == BLOCKS / 2 {
            controls.set_position(Azimuth(-110.0), Elevation(-20.0));
        }
        let at = b * BLOCK..(b + 1) * BLOCK;
        let out = drive(
            &mut node,
            RATE,
            &[&planes[0][at.clone()], &planes[1][at]],
            &[],
        );
        for plane in out {
            for s in plane {
                for byte in s.to_bits().to_le_bytes() {
                    digest ^= u64::from(byte);
                    digest = digest.wrapping_mul(0x0100_0000_01b3);
                }
            }
        }
    }
    digest
}

#[test]
fn the_stereo_panner_renders_what_it_always_did() {
    let digest = digest_of(VbapPannerNode::stereo().unwrap());
    eprintln!("DIGEST vbap stereo {digest:#018x}");
    assert_golden("vbap stereo", digest, 0xc090_25c1_9ff4_8d88);
}

#[test]
fn the_surround_panner_renders_what_it_always_did() {
    let digest = digest_of(VbapPannerNode::surround_5_1().unwrap());
    eprintln!("DIGEST vbap 5.1 {digest:#018x}");
    assert_golden("vbap 5.1", digest, 0xf949_5006_9f51_c9ae);
}
