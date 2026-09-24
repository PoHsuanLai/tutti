//! Golden renders of `VbapPannerNode::process`, pinned from the per-frame
//! implementation that solved the gains twice per frame.
//!
//! The panner now solves once per block and linearly ramps the gain vector
//! from the previous block's end to this block's end (design doc 013,
//! "`VbapPannerNode` `process`"). `GOLDENS` and `TICK_GOLDEN` were rendered by
//! the **old** per-frame solver (at `9f3acfad`, before the rewrite), so this
//! file is the equivalence proof and then stays as the regression pin. The
//! `print_*` generators render whatever is current: rerunning them now would
//! pin the new code against itself, so only do that for a deliberate change.
//!
//! Two tolerances, and why they differ:
//!
//! - **The last frame of every block is exact** (`EDGE_TOL`, 1e-6 — f32
//!   rounding only; measured ≤ 5e-9). The smoother is still stepped once per
//!   frame, so at the block's last frame the new code solves at the very
//!   position the old code solved at, and the ramp is written so its last
//!   sample *is* that solve.
//! - **Frames inside a block are within `RAMP_TOL`**. They lie on a chord of
//!   the gain curve where the old code sat on the curve itself, so the error is
//!   bounded by how far the source travels in one block. The 50 ms de-zipper
//!   moves at most `1 - exp(-64 / 2400)` ≈ 2.6% of the remaining arc per
//!   64-frame block: ≈ 3.2° on the 120° jump below, and up to ≈ 3.5° in the
//!   sweep, whose target runs away 25° a block. VBAP gains are piecewise smooth
//!   in the bearing with slope at most ≈ 2 per radian on these layouts (the
//!   30° C–L pair is the steepest), and a chord across a span Δθ of a function
//!   with slope ≤ L deviates by at most L·Δθ/2 ≈ 0.06 on a ±1 input. Measured
//!   against the full old render (every frame, not just the pinned ones): 0.0
//!   on both static cases, 1.3e-3 on the stereo jump, 5.5e-3 on the 5.1 jump,
//!   1.0e-3 on the mono fold and 0.021 on the sweep. `RAMP_TOL` is the 0.06
//!   bound, not the measurement, so it cannot be tuned to pass.
//!
//! One case differs **on purpose** inside its blocks: `stereo_width_switch`.
//! The old code switched between the mono-fold and two-source branches at the
//! block boundary with a hard step (0.64 of full scale, measured); the new
//! code crossfades the two across the next block. Only its block edges are
//! compared, and `a_width_crossing_the_fold_threshold_crossfades` pins the
//! crossfade itself.

use tutti_core::{AudioUnit, Azimuth, BufferVec, ChannelLayout, Elevation, Spread, StereoWidth};
use tutti_spatial::VbapPannerNode;

const BLOCK: usize = 64;
const BLOCKS: usize = 6;
/// Frame indices pinned per block. 63 is the block edge (exact), the others
/// sit inside the ramp.
const PINNED: [usize; 3] = [0, 31, 63];
const EDGE_TOL: f32 = 1e-6;
const RAMP_TOL: f32 = 0.06;

/// Deterministic, decorrelated L/R input, so the width path's two virtual
/// sources are distinguishable in the output.
fn input_block(block: usize) -> BufferVec {
    let mut buf = BufferVec::new(2);
    let mut state = 0x9e37_79b9_u32 ^ (block as u32).wrapping_mul(0x85eb_ca6b);
    for c in 0..2 {
        for i in 0..BLOCK {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            buf.set_f32(c, i, (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0);
        }
    }
    buf
}

/// A scenario: the layout, one-time setup, and a per-block control write.
struct Case {
    name: &'static str,
    speakers: u16,
    setup: fn(&mut VbapPannerNode),
    per_block: fn(&mut VbapPannerNode, usize),
    /// Whether frames inside a block are expected to follow the old render.
    /// False only where the old render stepped and the new one ramps.
    in_block: bool,
}

fn nothing(_: &mut VbapPannerNode, _: usize) {}

fn cases() -> Vec<Case> {
    vec![
        // Seated (reset) and held: the gains never move.
        Case {
            name: "stereo_static",
            speakers: 2,
            setup: |n| {
                n.set_position(Azimuth(20.0), Elevation::LEVEL);
                n.reset();
            },
            per_block: nothing,
            in_block: true,
        },
        // Seated at the front, then a 120° jump: the de-zipper is in flight
        // across every pinned block. 120° folds to 60° on a front-only pair.
        Case {
            name: "stereo_jump",
            speakers: 2,
            setup: |n| {
                n.set_position(Azimuth::FRONT, Elevation::LEVEL);
                n.reset();
                n.set_position(Azimuth(120.0), Elevation::LEVEL);
            },
            per_block: nothing,
            in_block: true,
        },
        Case {
            name: "surround_static_spread",
            speakers: 6,
            setup: |n| {
                n.set_position(Azimuth(45.0), Elevation::LEVEL);
                n.set_spread(Spread(0.3));
                n.reset();
            },
            per_block: nothing,
            in_block: true,
        },
        Case {
            name: "surround_jump",
            speakers: 6,
            setup: |n| {
                n.set_position(Azimuth::FRONT, Elevation::LEVEL);
                n.reset();
                n.set_position(Azimuth(120.0), Elevation::LEVEL);
            },
            per_block: nothing,
            in_block: true,
        },
        // Width 0 is the mono-fold branch; a fresh node ramps in from front.
        Case {
            name: "surround_mono_fold_moving",
            speakers: 6,
            setup: |n| {
                n.set_width(StereoWidth(0.0));
                n.set_position(Azimuth(-70.0), Elevation::LEVEL);
            },
            per_block: nothing,
            in_block: true,
        },
        // A new bearing every block, the way automation drives it.
        Case {
            name: "surround_sweep",
            speakers: 6,
            setup: |n| {
                n.set_position(Azimuth(10.0), Elevation::LEVEL);
                n.reset();
            },
            per_block: |n, b| n.set_position(Azimuth(10.0 + 25.0 * b as f32), Elevation(5.0)),
            in_block: true,
        },
        // The width crosses the mono-fold threshold mid-run.
        Case {
            name: "stereo_width_switch",
            speakers: 2,
            setup: |n| {
                n.set_position(Azimuth(-15.0), Elevation::LEVEL);
                n.reset();
            },
            per_block: |n, b| {
                n.set_width(StereoWidth(if b % 2 == 0 { 1.0 } else { 0.0 }));
            },
            in_block: false,
        },
    ]
}

/// Render a case: `BLOCKS` blocks of `process`, every channel, every frame.
/// Indexed `[block][channel][frame]`.
fn render(case: &Case) -> Vec<Vec<Vec<f32>>> {
    let mut node = VbapPannerNode::for_layout(ChannelLayout::from(case.speakers)).unwrap();
    node.set_sample_rate(tutti_core::SampleRate(48_000.0));
    (case.setup)(&mut node);
    let mut out = BufferVec::new(node.outputs());
    (0..BLOCKS)
        .map(|b| {
            (case.per_block)(&mut node, b);
            node.process(BLOCK, &input_block(b).buffer_ref(), &mut out.buffer_mut());
            (0..node.outputs())
                .map(|c| (0..BLOCK).map(|i| out.at_f32(c, i)).collect())
                .collect()
        })
        .collect()
}

/// The pinned subset of a render, flattened block → frame → channel.
fn pinned(render: &[Vec<Vec<f32>>]) -> Vec<(usize, usize, usize, f32)> {
    let mut v = Vec::new();
    for (b, block) in render.iter().enumerate() {
        for &i in &PINNED {
            for (c, ch) in block.iter().enumerate() {
                v.push((b, i, c, ch[i]));
            }
        }
    }
    v
}

/// Regenerates the tables below. Run against the implementation to be pinned:
/// `cargo nextest run -p tutti-spatial --run-ignored only print_goldens --no-capture`.
#[test]
#[ignore = "generator, not a check"]
fn print_goldens() {
    for case in cases() {
        let vals: Vec<String> = pinned(&render(&case))
            .iter()
            .map(|&(_, _, _, x)| format!("{x:?}"))
            .collect();
        println!("(\"{}\", &[{}]),", case.name, vals.join(", "));
    }
}

/// Every scenario reproduces the per-sample solver: exactly at each block's
/// last frame, within the chord bound inside it.
///
/// Mutation: solving at the block's *first* frame (stepping the smoother after
/// the solve) fails the edge checks of every moving case; holding the end
/// gains across the block (no ramp) fails the in-block checks — `stereo_jump`
/// block 2 frame 0 lands 0.068 off, past the 0.06 bound.
#[test]
fn process_matches_the_per_sample_solver() {
    let goldens: std::collections::HashMap<&str, &[f32]> = GOLDENS.iter().copied().collect();
    for case in cases() {
        let want = goldens[case.name];
        let got = pinned(&render(&case));
        assert_eq!(got.len(), want.len(), "{}: shape", case.name);
        for (&(b, i, c, x), &w) in got.iter().zip(want) {
            if i != BLOCK - 1 && !case.in_block {
                continue;
            }
            let tol = if i == BLOCK - 1 { EDGE_TOL } else { RAMP_TOL };
            assert!(
                (x - w).abs() <= tol,
                "{}: block {b} frame {i} ch {c}: {x} vs pinned {w} (tol {tol})",
                case.name
            );
        }
    }
}

/// `tick` is a block of one, so it must reproduce the per-sample solver bit
/// for bit — the ramp collapses to its end point.
///
/// Mutation: solving at the block's first frame instead of its last fails
/// here. (Writing the ramp as `from + (to - from) * 1.0` does *not*: frame to
/// frame the two gains are within a factor of two, so by Sterbenz the
/// difference is exact and the sum lands on `to` anyway. The subtraction form
/// in `render_channel` is exact without relying on that.)
#[test]
fn tick_matches_the_per_sample_solver() {
    let mut node = VbapPannerNode::surround_5_1().unwrap();
    node.set_sample_rate(tutti_core::SampleRate(48_000.0));
    node.set_position(Azimuth(100.0), Elevation(10.0));
    let mut got = Vec::new();
    let mut out = [0.0f32; 6];
    for k in 0..40 {
        let l = ((k * 37 % 17) as f32 / 8.5) - 1.0;
        let r = ((k * 53 % 19) as f32 / 9.5) - 1.0;
        node.tick(&[l, r], &mut out);
        if k % 13 == 0 || k == 39 {
            got.extend_from_slice(&out);
        }
    }
    let want: &[f32] = TICK_GOLDEN;
    assert_eq!(got.len(), want.len());
    for (n, (&x, &w)) in got.iter().zip(want).enumerate() {
        assert_eq!(x.to_bits(), w.to_bits(), "tick value {n}: {x} vs {w}");
    }
}

#[test]
#[ignore = "generator, not a check"]
fn print_tick_golden() {
    let mut node = VbapPannerNode::surround_5_1().unwrap();
    node.set_sample_rate(tutti_core::SampleRate(48_000.0));
    node.set_position(Azimuth(100.0), Elevation(10.0));
    let mut got = Vec::new();
    let mut out = [0.0f32; 6];
    for k in 0..40 {
        let l = ((k * 37 % 17) as f32 / 8.5) - 1.0;
        let r = ((k * 53 % 19) as f32 / 9.5) - 1.0;
        node.tick(&[l, r], &mut out);
        if k % 13 == 0 || k == 39 {
            got.extend_from_slice(&out);
        }
    }
    let vals: Vec<String> = got.iter().map(|x| format!("{x:?}")).collect();
    println!("const TICK_GOLDEN: &[f32] = &[{}];", vals.join(", "));
}

const TICK_GOLDEN: &[f32] = &[
    -0.7090228,
    -0.70518553,
    -1.4142084,
    0.0,
    -0.0,
    -0.0,
    -0.30197617,
    -0.3220242,
    -0.62731564,
    0.0,
    -0.0,
    -0.0,
    0.13352337,
    0.03441269,
    0.15520646,
    0.0,
    0.0,
    0.0,
    0.595517,
    0.36319155,
    0.93058056,
    0.0,
    0.0,
    0.0,
];

const GOLDENS: &[(&str, &[f32])] = &[
    (
        "stereo_static",
        &[
            0.2563849,
            0.5411052,
            0.0101596415,
            -0.24326196,
            0.07597178,
            -0.59005874,
            -0.37428236,
            0.24347013,
            -0.51883465,
            -0.50914913,
            0.40809906,
            0.078147046,
            -0.48572218,
            -0.58409786,
            0.48952186,
            0.5898652,
            -1.0834433,
            -0.09205608,
            -0.13902426,
            0.4410962,
            -0.04501786,
            0.06085164,
            1.270315,
            0.3392035,
            1.1221508,
            0.48646206,
            -0.14260614,
            -0.50145376,
            0.5810361,
            0.078902975,
            0.03432724,
            0.15947697,
            -1.0118762,
            -0.56775576,
            -0.17624116,
            0.5201577,
        ],
    ),
    (
        "stereo_jump",
        &[
            -0.13440755,
            0.69250417,
            0.16685446,
            -0.2752093,
            0.42429015,
            -0.6683837,
            -0.50541615,
            0.1853144,
            -0.22917515,
            -0.72182024,
            0.36238402,
            0.1719926,
            -0.19277921,
            -0.7931468,
            0.22791332,
            0.7893733,
            -1.0403793,
            -0.24497333,
            -0.30411837,
            0.51803905,
            -0.06453494,
            0.069758125,
            1.176522,
            0.4857025,
            0.9897437,
            0.65393203,
            -0.033634424,
            -0.6072999,
            0.5679464,
            0.09431535,
            0.008122042,
            0.18997364,
            -0.94554824,
            -0.6482022,
            -0.21299326,
            0.56666046,
        ],
    ),
    (
        "surround_static_spread",
        &[
            0.4926804,
            0.06969362,
            0.06969362,
            0.0,
            -0.12985682,
            0.06969362,
            -0.12291232,
            -0.012971502,
            -0.012971502,
            0.0,
            0.12909879,
            -0.012971502,
            -0.2577939,
            -0.023949891,
            -0.023949891,
            0.0,
            0.3420853,
            -0.023949891,
            -0.1631715,
            -0.040319704,
            -0.040319704,
            0.0,
            -0.33451757,
            -0.040319704,
            -0.68176717,
            -0.106214,
            -0.106214,
            0.0,
            -0.034333825,
            -0.106214,
            0.36281106,
            0.06438606,
            0.06438606,
            0.0,
            0.19047731,
            0.06438606,
            -0.6960707,
            -0.105821446,
            -0.105821446,
            0.0,
            0.022347108,
            -0.105821446,
            0.70216054,
            0.10672046,
            0.10672046,
            0.0,
            -0.023129523,
            0.10672046,
            -0.90110743,
            -0.16407578,
            -0.16407578,
            0.0,
            -0.5642211,
            -0.16407578,
            0.12807965,
            0.0058658123,
            0.0058658123,
            0.0,
            -0.30209157,
            0.0058658123,
            -0.0026395991,
            -0.0029732827,
            -0.0029732827,
            0.0,
            -0.056244534,
            -0.0029732827,
            1.1809746,
            0.20612146,
            0.20612146,
            0.0,
            0.5442493,
            0.20612146,
            1.1437583,
            0.19318444,
            0.19318444,
            0.0,
            0.3860237,
            0.19318444,
            -0.3819155,
            -0.05068089,
            -0.05068089,
            0.0,
            0.17390262,
            -0.05068089,
            0.49914414,
            0.089747086,
            0.089747086,
            0.0,
            0.28760573,
            0.089747086,
            0.11279428,
            0.014503967,
            0.014503967,
            0.0,
            -0.06152302,
            0.014503967,
            -1.1008275,
            -0.1818733,
            -0.1818733,
            0.0,
            -0.2826165,
            -0.1818733,
            0.14137012,
            0.00511685,
            0.00511685,
            0.0,
            -0.36317262,
            0.00511685,
        ],
    ),
    (
        "surround_jump",
        &[
            -0.339097,
            0.64292294,
            0.31022742,
            0.0,
            0.0,
            0.0,
            0.26414454,
            -0.25857067,
            -0.103734806,
            0.0,
            0.0,
            0.0,
            0.7322803,
            -0.5470862,
            -0.34855157,
            0.0,
            0.0,
            0.0,
            -0.5897435,
            0.22469254,
            -0.042306036,
            0.0,
            0.0,
            0.0,
            0.15213364,
            -0.4019617,
            -0.6777614,
            0.0,
            0.0,
            0.0,
            0.27838433,
            0.05110739,
            0.23859605,
            0.0,
            0.0,
            0.0,
            0.2832883,
            -0.37956396,
            -0.79024386,
            0.0,
            0.0,
            0.0,
            -0.29563922,
            0.3090444,
            0.8484974,
            0.0,
            0.0,
            0.0,
            -0.93093336,
            -0.036954504,
            -0.37896925,
            0.0,
            -0.0,
            -0.0,
            -0.71685326,
            0.17544363,
            0.5485705,
            0.0,
            0.0,
            0.0,
            -0.12573326,
            0.017508615,
            0.07930064,
            0.0,
            0.0,
            0.0,
            0.80515003,
            0.06215599,
            0.6564854,
            0.0,
            0.0,
            0.0,
            0.45928144,
            0.08762356,
            0.86474746,
            0.0,
            0.0,
            0.0,
            0.5372341,
            -0.04407096,
            -0.816165,
            0.0,
            0.0,
            0.0,
            0.47394907,
            9.698215e-5,
            0.13336164,
            0.0,
            0.0,
            0.0,
            -0.18189967,
            0.0,
            0.26884916,
            0.0,
            -7.382826e-5,
            0.0,
            -0.28895178,
            -0.0,
            -0.95594376,
            0.0,
            -0.005883899,
            -0.0,
            -0.7900178,
            0.0,
            0.87214446,
            0.0,
            -0.043227036,
            0.0,
        ],
    ),
    (
        "surround_mono_fold_moving",
        &[
            0.0,
            0.00022117414,
            0.2171017,
            0.0,
            0.0,
            0.0,
            -0.0,
            -0.001161179,
            -0.034871385,
            0.0,
            -0.0,
            -0.0,
            -0.0,
            -0.004007166,
            -0.058830738,
            0.0,
            -0.0,
            -0.0,
            -0.0,
            -0.010161533,
            -0.14678514,
            0.0,
            -0.0,
            -0.0,
            -0.0,
            -0.03567895,
            -0.34121564,
            0.0,
            -0.0,
            -0.0,
            0.0,
            0.030657953,
            0.21471219,
            0.0,
            0.0,
            0.0,
            -0.0,
            -0.048297636,
            -0.3353747,
            0.0,
            -0.0,
            -0.0,
            0.0,
            0.06148341,
            0.33610478,
            0.0,
            0.0,
            0.0,
            -0.0,
            -0.12231547,
            -0.54324764,
            0.0,
            -0.0,
            -0.0,
            0.0,
            0.00065494014,
            0.0028914276,
            0.0,
            0.0,
            0.0,
            -0.0,
            -0.0032570793,
            -0.012078929,
            0.0,
            -0.0,
            -0.0,
            0.0,
            0.20854437,
            0.6586664,
            0.0,
            0.0,
            0.0,
            0.0,
            0.19432467,
            0.6108412,
            0.0,
            0.0,
            0.0,
            -0.0,
            -0.052862436,
            -0.1443205,
            0.0,
            -0.0,
            -0.0,
            0.0,
            0.11725988,
            0.279914,
            0.0,
            0.0,
            0.0,
            0.0,
            0.01681395,
            0.03997526,
            0.0,
            0.0,
            0.0,
            -0.0,
            -0.25701204,
            -0.54131055,
            0.0,
            -0.0,
            -0.0,
            -0.0,
            -0.0012502172,
            -0.0023400884,
            0.0,
            -0.0,
            -0.0,
        ],
    ),
    (
        "surround_sweep",
        &[
            -0.46815017,
            0.18424515,
            0.7968592,
            0.0,
            0.0,
            0.0,
            0.33330032,
            -0.08283017,
            -0.3329077,
            0.0,
            0.0,
            0.0,
            0.85872734,
            -0.20091373,
            -0.79713714,
            0.0,
            0.0,
            0.0,
            -0.6902646,
            0.08270505,
            0.26002654,
            0.0,
            0.0,
            0.0,
            0.16914737,
            -0.1604186,
            -0.8110292,
            0.0,
            0.0,
            0.0,
            0.29752395,
            0.02269164,
            0.18179685,
            0.0,
            0.0,
            0.0,
            0.3024744,
            -0.16870514,
            -0.91752297,
            0.0,
            0.0,
            0.0,
            -0.30781633,
            0.14283386,
            0.93941706,
            0.0,
            0.0,
            0.0,
            -0.95199627,
            -0.018054709,
            -0.26564804,
            0.0,
            -0.0,
            -0.0,
            -0.7327784,
            0.08557039,
            0.65378034,
            0.0,
            0.0,
            0.0,
            -0.12722537,
            0.007912952,
            0.09243626,
            0.0,
            0.0,
            0.0,
            0.80928445,
            0.023158766,
            0.6041686,
            0.0,
            0.0,
            0.0,
            0.4615742,
            0.03198863,
            0.8374808,
            0.0,
            0.0,
            0.0,
            0.53347296,
            0.0,
            -0.84534824,
            0.0,
            0.001444988,
            0.0,
            0.48061946,
            0.0,
            0.13284121,
            0.0,
            0.011874079,
            0.0,
            -0.1677659,
            0.0,
            0.26846936,
            0.0,
            -0.004723427,
            0.0,
            -0.3497303,
            -0.0,
            -0.95093477,
            0.0,
            -0.012849127,
            -0.0,
            -0.72094655,
            0.0,
            0.8624789,
            0.0,
            -0.07171255,
            0.0,
        ],
    ),
    (
        "stereo_width_switch",
        &[
            -0.33799824,
            0.57420707,
            0.24063843,
            -0.16945714,
            0.6199898,
            -0.37474233,
            -0.050574295,
            -0.13817155,
            -0.11792336,
            -0.32217258,
            0.07455022,
            0.20367499,
            0.21709166,
            -0.7675915,
            -0.2199397,
            0.77446616,
            -0.67776537,
            -0.83295524,
            0.0010190294,
            0.0027840403,
            -0.0043001077,
            -0.011748114,
            0.2374761,
            0.6487968,
            0.32663083,
            1.1467177,
            0.38039917,
            -0.464961,
            0.3351327,
            0.4681488,
            0.014906402,
            0.040725045,
            -0.20596837,
            -0.56271607,
            -0.0009119411,
            -0.0024914695,
        ],
    ),
];

/// A constant `(l, r)` block, so every output frame *is* a gain readout.
fn dc_block(l: f32, r: f32) -> BufferVec {
    let mut buf = BufferVec::new(2);
    for i in 0..BLOCK {
        buf.set_f32(0, i, l);
        buf.set_f32(1, i, r);
    }
    buf
}

/// Render `blocks` blocks of DC through `node`, calling `per_block` before
/// each, and return every channel's frames concatenated across blocks.
fn dc_stream(
    node: &mut VbapPannerNode,
    input: &BufferVec,
    blocks: usize,
    per_block: impl Fn(&VbapPannerNode, usize),
) -> Vec<Vec<f32>> {
    let mut out = BufferVec::new(node.outputs());
    let mut stream = vec![Vec::new(); node.outputs()];
    for b in 0..blocks {
        per_block(node, b);
        node.process(BLOCK, &input.buffer_ref(), &mut out.buffer_mut());
        for (c, s) in stream.iter_mut().enumerate() {
            s.extend((0..BLOCK).map(|i| out.at_f32(c, i)));
        }
    }
    stream
}

/// Assert no block boundary steps further than the ramp inside the block it
/// opens. A linear ramp's frame-to-frame step is constant within a block, and
/// the boundary is one more step of the *next* block's ramp — so a boundary
/// step larger than that block's in-block step is a discontinuity.
fn assert_boundaries_are_on_the_ramp(stream: &[Vec<f32>], what: &str) {
    for (c, s) in stream.iter().enumerate() {
        for b in 1..s.len() / BLOCK {
            let start = b * BLOCK;
            let boundary = (s[start] - s[start - 1]).abs();
            let in_block = (s[start + 1] - s[start]).abs();
            assert!(
                boundary <= in_block + 1e-6,
                "{what}: ch {c} steps {boundary} at the block {b} boundary, \
                 but only {in_block} per frame inside the block"
            );
        }
    }
}

/// With DC in, the output is the gain vector itself; across a jump and a
/// per-block sweep it must be continuous over block boundaries.
///
/// Mutation: holding the end gains across the block (no ramp) makes each
/// boundary the whole block's step while the in-block step is 0 — fails on
/// the first moving block.
#[test]
fn the_gain_ramp_is_continuous_across_block_boundaries() {
    for speakers in [2u16, 6] {
        for width in [0.0f32, 1.0] {
            let mut node = VbapPannerNode::for_layout(ChannelLayout::from(speakers)).unwrap();
            node.set_sample_rate(tutti_core::SampleRate(48_000.0));
            node.set_width(StereoWidth(width));
            node.set_position(Azimuth(-20.0), Elevation::LEVEL);
            node.reset();
            let stream = dc_stream(&mut node, &dc_block(1.0, 0.5), 24, |n, b| {
                // A 140° jump, then a sweep that moves the target every block.
                let az = if b < 12 {
                    120.0
                } else {
                    120.0 - 15.0 * (b - 11) as f32
                };
                n.set_position(Azimuth(az), Elevation::LEVEL);
            });
            assert_boundaries_are_on_the_ramp(
                &stream,
                &format!("{speakers} speakers, width {width}"),
            );
        }
    }
}

/// A width crossing the mono-fold threshold between blocks crossfades the two
/// branches across the next block instead of stepping at the boundary.
///
/// Measured before the rewrite: the old per-frame solver stepped by 0.64 of
/// full scale at every crossing of `stereo_width_switch`.
///
/// Mutation: rendering a mixed mono/stereo ramp at its end point only (no
/// crossfade when the branches differ) fails at the first crossing.
#[test]
fn a_width_crossing_the_fold_threshold_crossfades() {
    let mut node = VbapPannerNode::stereo().unwrap();
    node.set_sample_rate(tutti_core::SampleRate(48_000.0));
    node.set_position(Azimuth(-15.0), Elevation::LEVEL);
    node.reset();
    // L only: the two-source branch and the fold weigh it differently.
    let stream = dc_stream(&mut node, &dc_block(1.0, 0.0), 8, |n, b| {
        n.set_width(StereoWidth(if b % 2 == 0 { 1.0 } else { 0.0 }));
    });
    // The crossings must actually move the output, or the continuity check
    // below would pass on a node that ignores width.
    let moved = (stream[0][2 * BLOCK - 1] - stream[0][BLOCK - 1]).abs();
    assert!(
        moved > 0.1,
        "the width switch should change the gains, moved {moved}"
    );
    assert_boundaries_are_on_the_ramp(&stream, "width switch");
}

/// A position written between blocks takes effect in the next block, ramped:
/// the block starts one ramp step from where the previous block ended, and
/// its last frame is exactly the per-frame solve at that frame.
///
/// The reference is the same scenario driven by `tick`, which is a block of
/// one and therefore the per-frame solver itself (see
/// `tick_matches_the_per_sample_solver`).
///
/// Mutation: solving at the block's *first* frame (stepping the smoother after
/// the solve) fails the last-frame equality; holding the end gains across the
/// block (no ramp) fails the first-frame proximity.
#[test]
fn a_position_change_lands_in_the_next_block_as_a_ramp() {
    let setup = || {
        let mut n = VbapPannerNode::surround_5_1().unwrap();
        n.set_sample_rate(tutti_core::SampleRate(48_000.0));
        n.set_position(Azimuth(0.0), Elevation::LEVEL);
        n.reset();
        n
    };
    let input = dc_block(0.8, 0.8);

    let mut node = setup();
    let mut out = BufferVec::new(6);
    node.process(BLOCK, &input.buffer_ref(), &mut out.buffer_mut());
    let settled: Vec<f32> = (0..6).map(|c| out.at_f32(c, BLOCK - 1)).collect();
    // Seated and unchanged: the first block is flat.
    for (c, &s) in settled.iter().enumerate() {
        assert_eq!(out.at_f32(c, 0), s, "ch {c} moved without a change");
    }

    node.set_position(Azimuth(90.0), Elevation::LEVEL);
    node.process(BLOCK, &input.buffer_ref(), &mut out.buffer_mut());

    let mut reference = setup();
    let mut frame = [0.0f32; 6];
    for _ in 0..BLOCK {
        reference.tick(&[0.8, 0.8], &mut frame);
    }
    reference.set_position(Azimuth(90.0), Elevation::LEVEL);
    for _ in 0..BLOCK {
        reference.tick(&[0.8, 0.8], &mut frame);
    }

    let mut any_moved = false;
    for (c, &s) in settled.iter().enumerate() {
        let last = out.at_f32(c, BLOCK - 1);
        assert_eq!(
            last.to_bits(),
            frame[c].to_bits(),
            "ch {c}: the block must end on the per-frame solve"
        );
        let step = (last - s) / BLOCK as f32;
        let first = out.at_f32(c, 0);
        assert!(
            (first - (s + step)).abs() <= 1e-6,
            "ch {c}: the block must start one ramp step from {s}, got {first}"
        );
        any_moved |= (last - s).abs() > 1e-3;
    }
    assert!(any_moved, "the new position must take effect in this block");
}
