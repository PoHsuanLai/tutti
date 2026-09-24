//! The merged width-generic nodes rendered side by side with the twins they
//! replace, on the same input.
//!
//! **Bit-identical** wherever the old node's arithmetic is kept: every
//! configuration the twins supported, with the controls held, through `process`
//! (in ragged blocks) and through `tick`. Two paths are *not* bit-identical by
//! design, and are held to a stated tolerance instead:
//!
//! - a **moving** SVF/ladder cutoff (a swept param port): the coefficients are
//!   solved every [`COEFF_INTERVAL`](crate::ramp::COEFF_INTERVAL) samples and
//!   interpolated, where the old node solved a `tan` every sample;
//! - the **phaser**, whose LFO always moves: same scheme for its all-pass
//!   coefficient.
//!
//! `tick` is a block of one, so it solves every sample and stays bit-identical
//! even on those paths — which pins that the interpolation is the *only*
//! difference.

use tutti_core::{AudioUnit, BufferVec, ChannelLayout, SampleRate};

use super::{chorus, delay, flanger, ladder, phaser, svf};
use crate::{
    DelayLineNode, InterpolationMode, LadderFilterNode, LadderType, ModDelayNode, PhaserNode,
    SvfFilterNode, SvfType,
};

const SR: SampleRate = SampleRate(48_000.0);
const LEN: usize = 4096;
/// Ragged on purpose: full blocks, odd sizes, and a block of one.
const PATTERN: [usize; 6] = [64, 64, 17, 1, 64, 33];

/// Deterministic broadband input in `[-1, 1)`.
fn noise(seed: u32, len: usize) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        })
        .collect()
}

/// `n` independent noise channels.
fn noise_channels(n: usize) -> Vec<Vec<f32>> {
    (0..n).map(|c| noise(c as u32 + 1, LEN)).collect()
}

/// An exponential sweep `from → to` Hz over the render — a param-port signal
/// that moves every sample.
fn sweep(from: f32, to: f32) -> Vec<f32> {
    (0..LEN)
        .map(|i| from * (to / from).powf(i as f32 / LEN as f32))
        .collect()
}

fn render_process(node: &mut dyn AudioUnit, inputs: &[Vec<f32>]) -> Vec<Vec<f32>> {
    assert_eq!(inputs.len(), node.inputs(), "one input signal per port");
    let (nin, nout) = (node.inputs(), node.outputs());
    let mut out = vec![vec![0.0f32; LEN]; nout];
    let mut ib = BufferVec::new(nin);
    let mut ob = BufferVec::new(nout);
    let (mut pos, mut k) = (0, 0);
    while pos < LEN {
        let n = PATTERN[k % PATTERN.len()].min(LEN - pos);
        k += 1;
        for (c, sig) in inputs.iter().enumerate() {
            for i in 0..n {
                ib.set_f32(c, i, sig[pos + i]);
            }
        }
        node.process(n, &ib.buffer_ref(), &mut ob.buffer_mut());
        for (c, o) in out.iter_mut().enumerate() {
            for i in 0..n {
                o[pos + i] = ob.at_f32(c, i);
            }
        }
        pos += n;
    }
    out
}

fn render_tick(node: &mut dyn AudioUnit, inputs: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let (nin, nout) = (node.inputs(), node.outputs());
    let mut out = vec![vec![0.0f32; LEN]; nout];
    let mut frame_in = vec![0.0f32; nin];
    let mut frame_out = vec![0.0f32; nout];
    for i in 0..LEN {
        for (c, sig) in inputs.iter().enumerate() {
            frame_in[c] = sig[i];
        }
        node.tick(&frame_in, &mut frame_out);
        for (c, o) in out.iter_mut().enumerate() {
            o[i] = frame_out[c];
        }
    }
    out
}

#[track_caller]
fn assert_bits(new: &[Vec<f32>], old: &[Vec<f32>], what: &str) {
    assert_eq!(new.len(), old.len(), "{what}: width");
    for (c, (n, o)) in new.iter().zip(old).enumerate() {
        for (i, (a, b)) in n.iter().zip(o).enumerate() {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "{what}: channel {c} sample {i}: new {a} vs old {b}"
            );
        }
    }
}

fn max_diff(new: &[Vec<f32>], old: &[Vec<f32>]) -> f32 {
    new.iter()
        .zip(old)
        .flat_map(|(n, o)| n.iter().zip(o).map(|(a, b)| (a - b).abs()))
        .fold(0.0, f32::max)
}

/// Build a fresh pair, set both to the render rate, and compare `process`
/// and `tick` renders bit for bit.
fn both_paths_bit_identical(
    what: &str,
    make_new: impl Fn() -> Box<dyn AudioUnit>,
    make_old: impl Fn() -> Box<dyn AudioUnit>,
    inputs: &[Vec<f32>],
) {
    for (path, render) in [
        (
            "process",
            render_process as fn(&mut dyn AudioUnit, &[Vec<f32>]) -> _,
        ),
        ("tick", render_tick),
    ] {
        let (mut new, mut old) = (make_new(), make_old());
        new.set_sample_rate(SR);
        old.set_sample_rate(SR);
        let got = render(new.as_mut(), inputs);
        let want = render(old.as_mut(), inputs);
        assert_bits(&got, &want, &format!("{what} ({path})"));
    }
}

// ── SVF ──────────────────────────────────────────────────────────────────────

const SVF_TYPES: [SvfType; 8] = [
    SvfType::LowPass,
    SvfType::HighPass,
    SvfType::BandPass,
    SvfType::Notch,
    SvfType::Allpass,
    SvfType::Bell,
    SvfType::LowShelf,
    SvfType::HighShelf,
];

/// Mutation: dropping the `m2 * v2` term from `svf_step` fails (on the
/// low-pass, the first type whose `m2` is non-zero).
#[test]
fn svf_mono_is_the_old_mono_svf() {
    let input = noise_channels(1);
    for ty in SVF_TYPES {
        both_paths_bit_identical(
            &format!("svf f64 {ty:?}"),
            || Box::new(SvfFilterNode::<f64>::new(ty, 1_200.0, 0.9).with_gain_db(6.0)),
            || Box::new(svf::SvfFilterNode::<f64>::new(ty, 1_200.0, 0.9).with_gain_db(6.0)),
            &input,
        );
        both_paths_bit_identical(
            &format!("svf f32 {ty:?}"),
            || Box::new(SvfFilterNode::<f32>::new(ty, 300.0, 2.0).with_gain_db(-4.0)),
            || Box::new(svf::SvfFilterNode::<f32>::new(ty, 300.0, 2.0).with_gain_db(-4.0)),
            &input,
        );
    }
}

/// Mutation: indexing `ic1eq[0]` for every channel in `run_held` fails at
/// width 2 and 6.
#[test]
fn svf_wide_is_the_old_wide_svf() {
    for w in [2usize, 6] {
        let input = noise_channels(w);
        both_paths_bit_identical(
            &format!("svf width {w}"),
            || {
                Box::new(SvfFilterNode::<f64>::with_channels(
                    ChannelLayout::from(w),
                    SvfType::LowPass,
                    900.0,
                    0.8,
                ))
            },
            || {
                Box::new(svf::StereoSvfFilterNode::<f64>::with_channels(
                    w,
                    SvfType::LowPass,
                    900.0,
                    0.8,
                ))
            },
            &input,
        );
    }
}

/// Ports held at a constant are the unported filter, old and new alike.
#[test]
fn svf_ports_held_constant_are_the_old_ported_svf() {
    let mut input = noise_channels(2);
    input.push(vec![1_500.0; LEN]);
    input.push(vec![1.3; LEN]);
    both_paths_bit_identical(
        "svf ports held",
        || {
            Box::new(SvfFilterNode::<f64>::with_param_inputs(
                ChannelLayout::STEREO,
                SvfType::BandPass,
                1_000.0,
                0.7,
                true,
                true,
            ))
        },
        || {
            Box::new(svf::StereoSvfFilterNode::<f64>::with_param_inputs(
                2,
                SvfType::BandPass,
                1_000.0,
                0.7,
                true,
                true,
            ))
        },
        &input,
    );
}

/// A cutoff port sweeping 200 Hz → 8 kHz — 40× in 85 ms, far faster than any
/// musical automation: `tick` still solves per sample (bit-identical),
/// `process` interpolates between solves every 16 samples.
///
/// The tolerance is measured, then bounded from both sides. With the interval
/// set to 1 sample this render is bit-identical to the old node, so the
/// interpolation is the *only* difference. At 16 samples the worst sample
/// differs by 3.0e-5 on unit-amplitude noise; the error grows with the square
/// of the interval (4.6e-4 at 64). `1e-4` sits 3× above the measurement and
/// ~5× below a 64-sample interval.
///
/// Mutation: `COEFF_INTERVAL = 64` exceeds the tolerance.
#[test]
fn svf_swept_cutoff_interpolates_within_tolerance() {
    let mut input = noise_channels(2);
    input.push(sweep(200.0, 8_000.0));
    let make_new = || {
        SvfFilterNode::<f64>::with_param_inputs(
            ChannelLayout::STEREO,
            SvfType::LowPass,
            1_000.0,
            0.707,
            true,
            false,
        )
    };
    let make_old = || {
        svf::StereoSvfFilterNode::<f64>::with_param_inputs(
            2,
            SvfType::LowPass,
            1_000.0,
            0.707,
            true,
            false,
        )
    };

    let (mut new, mut old) = (make_new(), make_old());
    new.set_sample_rate(SR);
    old.set_sample_rate(SR);
    assert_bits(
        &render_tick(&mut new, &input),
        &render_tick(&mut old, &input),
        "svf sweep (tick)",
    );

    let (mut new, mut old) = (make_new(), make_old());
    new.set_sample_rate(SR);
    old.set_sample_rate(SR);
    let d = max_diff(
        &render_process(&mut new, &input),
        &render_process(&mut old, &input),
    );
    eprintln!("svf sweep process max diff {d:e}");
    assert!(d < 1e-4, "svf sweep diverged by {d}");
}

// ── Ladder ───────────────────────────────────────────────────────────────────

const LADDER_TYPES: [LadderType; 4] = [
    LadderType::LP12,
    LadderType::LP24,
    LadderType::HP12,
    LadderType::HP24,
];

/// Mutation: reading stage 2 as the LP12 tap in `ladder_step` fails LP12/HP12.
#[test]
fn ladder_mono_is_the_old_mono_ladder() {
    let input = noise_channels(1);
    for ty in LADDER_TYPES {
        both_paths_bit_identical(
            &format!("ladder {ty:?}"),
            || {
                let n = LadderFilterNode::<f64>::new(ty, 1_100.0, 0.7);
                n.set_drive(2.5);
                Box::new(n)
            },
            || {
                let n = ladder::LadderFilterNode::<f64>::new(ty, 1_100.0, 0.7);
                n.set_drive(2.5);
                Box::new(n)
            },
            &input,
        );
    }
}

/// Mutation: sharing one channel's stage lanes across channels in `run_held`
/// fails.
#[test]
fn ladder_wide_is_the_old_wide_ladder() {
    for w in [2usize, 6] {
        let input = noise_channels(w);
        both_paths_bit_identical(
            &format!("ladder width {w}"),
            || {
                Box::new(LadderFilterNode::<f32>::with_channels(
                    ChannelLayout::from(w),
                    LadderType::LP24,
                    700.0,
                    0.5,
                ))
            },
            || {
                Box::new(ladder::StereoLadderFilterNode::<f32>::with_channels(
                    w,
                    LadderType::LP24,
                    700.0,
                    0.5,
                ))
            },
            &input,
        );
    }
}

/// A drive port is audio, read per sample by both — bit-identical even though
/// it moves; so are held cutoff/Q ports.
#[test]
fn ladder_drive_port_and_held_ports_are_the_old_ported_ladder() {
    let mut input = noise_channels(2);
    input.push(vec![900.0; LEN]);
    input.push(vec![0.6; LEN]);
    input.push(
        (0..LEN)
            .map(|i| 1.0 + 3.0 * (i as f32 / LEN as f32))
            .collect(),
    );
    both_paths_bit_identical(
        "ladder ports",
        || {
            Box::new(LadderFilterNode::<f64>::with_param_inputs(
                ChannelLayout::STEREO,
                LadderType::LP24,
                1_000.0,
                0.3,
                true,
                true,
                true,
            ))
        },
        || {
            Box::new(ladder::StereoLadderFilterNode::<f64>::with_param_inputs(
                2,
                LadderType::LP24,
                1_000.0,
                0.3,
                true,
                true,
                true,
            ))
        },
        &input,
    );
}

/// As [`svf_swept_cutoff_interpolates_within_tolerance`], for the ladder:
/// bit-identical at a 1-sample interval, 3.8e-5 worst at 16, 7.8e-4 at 64.
///
/// Mutation: `COEFF_INTERVAL = 64` exceeds the tolerance.
#[test]
fn ladder_swept_cutoff_interpolates_within_tolerance() {
    let mut input = noise_channels(2);
    input.push(sweep(200.0, 8_000.0));
    let make_new = || {
        LadderFilterNode::<f64>::with_param_inputs(
            ChannelLayout::STEREO,
            LadderType::LP24,
            1_000.0,
            0.6,
            true,
            false,
            false,
        )
    };
    let make_old = || {
        ladder::StereoLadderFilterNode::<f64>::with_param_inputs(
            2,
            LadderType::LP24,
            1_000.0,
            0.6,
            true,
            false,
            false,
        )
    };
    let (mut new, mut old) = (make_new(), make_old());
    new.set_sample_rate(SR);
    old.set_sample_rate(SR);
    assert_bits(
        &render_tick(&mut new, &input),
        &render_tick(&mut old, &input),
        "ladder sweep (tick)",
    );
    let (mut new, mut old) = (make_new(), make_old());
    new.set_sample_rate(SR);
    old.set_sample_rate(SR);
    let d = max_diff(
        &render_process(&mut new, &input),
        &render_process(&mut old, &input),
    );
    eprintln!("ladder sweep process max diff {d:e}");
    assert!(d < 1e-4, "ladder sweep diverged by {d}");
}

// ── Delay ────────────────────────────────────────────────────────────────────

/// Mutation: reading the feedback tap at `d` rather than `d - 1` in
/// `delay_step` fails.
#[test]
fn delay_mono_is_the_old_mono_delay() {
    let input = noise_channels(1);
    for interp in [
        InterpolationMode::None,
        InterpolationMode::Linear,
        InterpolationMode::CubicHermite,
    ] {
        both_paths_bit_identical(
            &format!("delay mono {interp:?}"),
            || {
                let n = DelayLineNode::new(0.5, 0.012_34, 0.6).with_interpolation(interp);
                n.set_mix(0.4);
                Box::new(n)
            },
            || {
                let n = delay::DelayLineNode::new(0.5, 0.012_34, 0.6).with_interpolation(interp);
                n.set_mix(0.4);
                Box::new(n)
            },
            &input,
        );
    }
}

/// The width-2 cross-feed as a routing matrix is the old L↔R special case.
///
/// Mutation: routing each channel's tap into *itself* (identity matrix) at
/// width 2 fails.
#[test]
fn delay_stereo_cross_feed_is_the_old_stereo_delay() {
    let input = noise_channels(2);
    both_paths_bit_identical(
        "delay stereo",
        || {
            let n = DelayLineNode::stereo(0.5, 0.010, 0.017, 0.5);
            n.set_cross_feedback(0.3);
            n.set_mix(0.7);
            Box::new(n)
        },
        || {
            let n = delay::StereoDelayLineNode::new(0.5, 0.010, 0.017, 0.5);
            n.set_cross_feedback(0.3);
            n.set_mix(0.7);
            Box::new(n)
        },
        &input,
    );
}

#[test]
fn delay_wide_is_the_old_wide_delay() {
    let input = noise_channels(6);
    both_paths_bit_identical(
        "delay width 6",
        || {
            let n = DelayLineNode::with_channels(6usize, 0.5, 0.011, 0.45);
            n.set_cross_feedback(0.9); // inert: no routing at width 6
            n.set_mix(0.6);
            Box::new(n)
        },
        || {
            let n = delay::StereoDelayLineNode::with_channels(6, 0.5, 0.011, 0.45);
            n.set_cross_feedback(0.9);
            n.set_mix(0.6);
            Box::new(n)
        },
        &input,
    );
}

/// Feedback and delay-time ports are audio in both; moving, they still match.
#[test]
fn delay_ports_are_the_old_ported_delay() {
    for w in [2usize, 6] {
        let mut input = noise_channels(w);
        input.push((0..LEN).map(|i| 0.9 * (i as f32 / LEN as f32)).collect());
        input.push(
            (0..LEN)
                .map(|i| 0.002 + 0.01 * (i as f32 / LEN as f32))
                .collect(),
        );
        both_paths_bit_identical(
            &format!("delay ports width {w}"),
            || {
                let n = DelayLineNode::with_param_inputs(
                    ChannelLayout::from(w),
                    0.5,
                    0.01,
                    0.4,
                    true,
                    true,
                );
                n.set_cross_feedback(0.2);
                Box::new(n)
            },
            || {
                let n =
                    delay::StereoDelayLineNode::with_param_inputs(w, 0.5, 0.01, 0.4, true, true);
                n.set_cross_feedback(0.2);
                Box::new(n)
            },
            &input,
        );
    }
}

// ── Chorus / flanger ─────────────────────────────────────────────────────────

/// Mutation: giving channel 1 the flanger's offset on the chorus preset (or
/// dropping the per-channel offset) fails.
#[test]
fn mod_delay_presets_are_the_old_chorus_and_flanger() {
    let input = noise_channels(2);
    both_paths_bit_identical(
        "chorus",
        || {
            let n = ModDelayNode::chorus(ChannelLayout::STEREO);
            n.set_rate(1.7);
            n.set_depth(0.007);
            n.set_feedback(0.45);
            n.set_mix(0.6);
            Box::new(n)
        },
        || {
            let n = chorus::ChorusNode::new();
            n.set_rate(1.7);
            n.set_depth(0.007);
            n.set_feedback(0.45);
            n.set_mix(0.6);
            Box::new(n)
        },
        &input,
    );
    both_paths_bit_identical(
        "flanger",
        || Box::new(ModDelayNode::flanger(ChannelLayout::STEREO)),
        || Box::new(flanger::FlangerNode::new()),
        &input,
    );
}

// ── Phaser ───────────────────────────────────────────────────────────────────

/// The phaser's LFO always moves, so its coefficient is solved every 16
/// samples and interpolated: `tick` is bit-identical, `process` is within a
/// tolerance. Bit-identical at a 1-sample interval; at 16 the worst sample
/// differs by 3.5e-6 (2 Hz, stereo), at 64 by 6.1e-5. `1e-5` sits between.
/// (The 0.3 Hz mono case sits at the 1e-6 rounding floor either way.)
///
/// Mutation: `COEFF_INTERVAL = 64` exceeds the tolerance on the 2 Hz case;
/// dropping `solve_first` (ramping the first block in from a stand-in) puts
/// it at 1.5e-4.
#[test]
fn phaser_is_the_old_phaser_within_interpolation() {
    for (w, stages, rate) in [(1usize, 6usize, 0.3f32), (2, 4, 2.0)] {
        let input = noise_channels(w);
        let make_new = || {
            let n = PhaserNode::with_channels(ChannelLayout::from(w), stages);
            n.set_rate(rate);
            n.set_depth(0.8);
            n.set_feedback(0.6);
            n
        };
        let make_old = || -> Box<dyn AudioUnit> {
            if w == 1 {
                let n = phaser::PhaserNode::new(stages);
                n.set_rate(rate);
                n.set_depth(0.8);
                n.set_feedback(0.6);
                Box::new(n)
            } else {
                let n = phaser::StereoPhaserNode::new(stages);
                n.set_rate(rate);
                n.set_depth(0.8);
                n.set_feedback(0.6);
                Box::new(n)
            }
        };
        let (mut new, mut old) = (make_new(), make_old());
        new.set_sample_rate(SR);
        old.set_sample_rate(SR);
        assert_bits(
            &render_tick(&mut new, &input),
            &render_tick(old.as_mut(), &input),
            &format!("phaser width {w} (tick)"),
        );
        let (mut new, mut old) = (make_new(), make_old());
        new.set_sample_rate(SR);
        old.set_sample_rate(SR);
        let d = max_diff(
            &render_process(&mut new, &input),
            &render_process(old.as_mut(), &input),
        );
        eprintln!("phaser width {w} process max diff {d:e}");
        assert!(d < 1e-5, "phaser width {w} diverged by {d}");
    }
}
