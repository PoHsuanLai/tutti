//! Every VBAP preset's render, hashed (FNV-1a over the bits of every output
//! sample): the stereo pair, quad, 5.1, 7.1 and Atmos 7.1.4 each pan seeded
//! noise through a bearing and height sweep that crosses the seam, the poles
//! and the horizon, with the stereo width and the spread switched mid-render,
//! so the tuple choice, the retreat, the width branch, the spread and the
//! block ramp are all in the digest.
//!
//! The first four are the figures the panners rendered with the `vbap` crate
//! under them (design 028 section 4.3, PR 1: the solve moved into
//! `tutti-dsp`, bit for bit). Atmos 7.1.4's is the one that moved: the crate's
//! triplets for it left the front of the array and the wall between the beds
//! uncovered (its centre speaker was in no triplet, and a horizontal-plane
//! source reached the speakers only through the elevation retreat), and
//! listed each coplanar square twice over (overlapping triangles), so the
//! tiling the method describes changed what 7.1.4 renders above the horizon.
//! The new tuples are pinned in `tutti-dsp`'s `spatial` tests.
//!
//! The gains reach `libm` (`sincos`, `atan2` in the solve and the smoother),
//! so the digests go through `assert_golden`.
//!
//! Mutation (run): the second and third columns of a triplet's matrix swapped
//! in `SpeakerLayout::new` moves the 7.1.4 digest; a `norm` fixed at 1 moves
//! all five.

use tutti_graph::contract::{drive, prepared};
use tutti_nodes::VbapPannerNode;
use tutti_nodes::testing::assert_golden;
use tutti_types::{Azimuth, Elevation, SampleRate, Spread, StereoWidth};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;
const BLOCKS: usize = 120;

fn noise(seed: u32) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..BLOCK * BLOCKS)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        })
        .collect()
}

fn digest_of(node: VbapPannerNode) -> u64 {
    let controls = node.controls();
    controls.set_position(Azimuth(170.0), Elevation(10.0));
    let mut node = prepared(node, RATE, BLOCK);
    let planes = [noise(3), noise(4)];
    let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
    let moves = [
        (-110.0f32, -20.0f32),
        (45.0, 45.0),
        (0.0, 90.0),
        (-30.0, 0.0),
        (90.0, 30.0),
        (135.0, -60.0),
        (180.0, 0.0),
        (10.0, 5.0),
    ];
    for b in 0..BLOCKS {
        if b % 12 == 6 {
            let (az, el) = moves[(b / 12) % moves.len()];
            controls.set_position(Azimuth(az), Elevation(el));
        }
        match b {
            30 => controls.set_width(StereoWidth::NATURAL),
            60 => controls.set_spread(Spread::new_clamped(0.4)),
            90 => controls.set_width(StereoWidth::MONO),
            _ => {}
        }
        let at = b * BLOCK..(b + 1) * BLOCK;
        for plane in drive(
            &mut node,
            RATE,
            &[&planes[0][at.clone()], &planes[1][at]],
            &[],
        ) {
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

fn check(name: &str, node: VbapPannerNode, want: u64) {
    let digest = digest_of(node);
    eprintln!("DIGEST {name} {digest:#018x}");
    assert_golden(name, digest, want);
}

#[test]
fn stereo_renders_what_it_did_under_the_crate() {
    check(
        "vbap stereo preset",
        VbapPannerNode::stereo().unwrap(),
        0xd842_5cdf_ac1f_cf37,
    );
}

#[test]
fn quad_renders_what_it_did_under_the_crate() {
    check(
        "vbap quad preset",
        VbapPannerNode::quad().unwrap(),
        0x2990_c3b4_832c_b888,
    );
}

#[test]
fn surround_5_1_renders_what_it_did_under_the_crate() {
    check(
        "vbap 5.1 preset",
        VbapPannerNode::surround_5_1().unwrap(),
        0xa795_3955_3bc4_b754,
    );
}

#[test]
fn surround_7_1_renders_what_it_did_under_the_crate() {
    check(
        "vbap 7.1 preset",
        VbapPannerNode::surround_7_1().unwrap(),
        0x974e_2551_e709_30e3,
    );
}

/// Atmos 7.1.4 under the tiling of its triangles (not the crate's, which was
/// `0x4444_fb33_257f_df44`).
#[test]
fn atmos_7_1_4_renders_under_the_tiled_triangles() {
    check(
        "vbap 7.1.4 preset",
        VbapPannerNode::atmos_7_1_4().unwrap(),
        0x13f2_9ba0_6e7b_db98,
    );
}
