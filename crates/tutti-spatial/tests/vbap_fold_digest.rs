//! The VBAP panner's mono-fold branch, rendered on two distinct seeded noises
//! and hashed (FNV-1a over the bits of every output sample): pins its output
//! across the move of its input fold from the width-only fold to
//! `tutti_types::Fold` (design 024 step 0b, consolidation 3). The stereo width
//! starts at mono (the fold branch), crosses to natural mid-render (the
//! crossfade into the pair branch) and the source moves, so the folded ramp,
//! the held-gain shortcut and the crossfade are all in the digest. The gains
//! reach `libm` (`sin`, `cos`), so the digest goes through `assert_golden`.
//!
//! Mutation (run): the fold's `0.5` -> `1.0` (the pair summed, not averaged)
//! changes the digest and fails.

use tutti_graph::contract::{drive, prepared};
use tutti_nodes::testing::assert_golden;
use tutti_spatial::VbapPannerNode;
use tutti_types::{Azimuth, Elevation, SampleRate, StereoWidth};

const RATE: SampleRate = SampleRate(48_000.0);

fn noise(seed: u32, len: usize) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        })
        .collect()
}

#[test]
fn the_panners_fold_renders_what_it_always_did() {
    let (l, r) = (noise(1, 1024), noise(2, 1024));
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for panner in [
        VbapPannerNode::stereo().unwrap(),
        VbapPannerNode::surround_5_1().unwrap(),
    ] {
        let controls = panner.controls();
        controls.set_width(StereoWidth::MONO);
        controls.set_position(Azimuth(20.0), Elevation(0.0));
        let mut node = prepared(panner, RATE, 64);
        for (i, (cl, cr)) in l.chunks(64).zip(r.chunks(64)).enumerate() {
            match i {
                4 => controls.set_position(Azimuth(-80.0), Elevation(0.0)),
                9 => controls.set_width(StereoWidth::NATURAL),
                12 => controls.set_width(StereoWidth::MONO),
                _ => {}
            }
            for out in drive(&mut node, RATE, &[cl, cr], &[]) {
                for s in out {
                    for byte in s.to_bits().to_le_bytes() {
                        h ^= u64::from(byte);
                        h = h.wrapping_mul(0x0100_0000_01b3);
                    }
                }
            }
        }
    }
    println!("DIGEST vbap {h:#018x}");
    assert_golden("the VBAP panner's folded render", h, 0);
}
