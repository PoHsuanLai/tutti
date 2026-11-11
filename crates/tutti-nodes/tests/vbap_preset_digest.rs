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
//!
//! These digests also carry the deleted `spatial_smoother_digest` and
//! `vbap_fold_digest` tests. Mutations run against them (each moves the
//! digests named, so each deleted test was safe to delete):
//! `Smoother::gap_closed` with `powi(steps + 1)` moves all five; the stereo
//! to mono fold's `scale: 0.5` -> `1.0` in `tutti-types`' `Fold` moves all
//! five; the mono branch's held-gain shortcut in `VbapGains::render_channel`
//! as `mono(l, r) * (g_to + 1e-4)` moves the stereo digest; `pair()`'s `half`
//! as `* 0.6` (the crossfade out of and into the mono branch) moves all five.

use tutti_fixtures::Fnv;
use tutti_graph::ParamNode;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::VbapPannerNode;
use tutti_nodes::testing::aim;
use tutti_nodes::testing::assert_golden;
use tutti_types::{Azimuth, Elevation, SampleRate, StereoWidth, UnitParam};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;
const BLOCKS: usize = 120;

fn noise(seed: u32) -> Vec<f32> {
    tutti_fixtures::noise(seed, BLOCK * BLOCKS)
}

fn digest_of(node: VbapPannerNode) -> u64 {
    let controls = node.param_set();
    aim(&controls, Azimuth(170.0), Elevation(10.0));
    let mut node = prepared(node, RATE, BLOCK);
    let planes = [noise(3), noise(4)];
    let mut digest = Fnv::new();
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
            aim(&controls, Azimuth(az), Elevation(el));
        }
        match b {
            30 => drop(controls.set(UnitParam::StereoWidth, StereoWidth::NATURAL.get())),
            60 => drop(controls.set(UnitParam::Spread, 0.4)),
            90 => drop(controls.set(UnitParam::StereoWidth, StereoWidth::MONO.get())),
            _ => {}
        }
        let at = b * BLOCK..(b + 1) * BLOCK;
        for plane in drive(
            &mut node,
            RATE,
            &[&planes[0][at.clone()], &planes[1][at]],
            &[],
        ) {
            digest.f32s(&plane);
        }
    }
    digest.finish()
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
        VbapPannerNode::stereo(),
        0xd842_5cdf_ac1f_cf37,
    );
}

#[test]
fn quad_renders_what_it_did_under_the_crate() {
    check(
        "vbap quad preset",
        VbapPannerNode::quad(),
        0x2990_c3b4_832c_b888,
    );
}

#[test]
fn surround_5_1_renders_what_it_did_under_the_crate() {
    check(
        "vbap 5.1 preset",
        VbapPannerNode::surround_5_1(),
        0xa795_3955_3bc4_b754,
    );
}

#[test]
fn surround_7_1_renders_what_it_did_under_the_crate() {
    check(
        "vbap 7.1 preset",
        VbapPannerNode::surround_7_1(),
        0x974e_2551_e709_30e3,
    );
}

/// Atmos 7.1.4 under the tiling of its triangles (not the crate's, which was
/// `0x4444_fb33_257f_df44`).
#[test]
fn atmos_7_1_4_renders_under_the_tiled_triangles() {
    check(
        "vbap 7.1.4 preset",
        VbapPannerNode::atmos_7_1_4(),
        0x13f2_9ba0_6e7b_db98,
    );
}
