//! `AudioUnit::latency()` must be the delay the output actually has — design
//! doc 013, defects D1 and D3.
//!
//! PDC reads nothing but that figure: it delays every *other* path by it. So a
//! figure that is too high (a musical delay reported as latency, D1) drags the
//! rest of the mix late, and a figure that is true of only part of the output
//! (the convolver's wet half, D3) leaves the other part early after
//! compensation. Each test therefore pins the reported number **and** measures
//! the output with an impulse, because the number alone cannot fail on a node
//! whose DSP drifted away from it.

use tutti_core::dsp::{Net, Source};
use tutti_core::{latency, AudioUnit, BufferVec, SampleRate, Signal, SignalFrame};
use tutti_nodes::testing::Through;
use tutti_nodes::{ChorusNode, DelayLineNode, FlangerNode, StereoDelayLineNode};

const SR: SampleRate = SampleRate(48_000.0);
/// `BufferVec`'s block length.
const BLOCK: usize = 64;

/// Run `unit` over `frames` frames of a unit impulse at frame 0 on every input,
/// through `process` in [`BLOCK`]-frame blocks. Returns one `Vec` per output.
fn impulse_response(unit: &mut dyn AudioUnit, frames: usize) -> Vec<Vec<f32>> {
    let mut input = BufferVec::new(unit.inputs());
    let mut output = BufferVec::new(unit.outputs());
    let mut out: Vec<Vec<f32>> = vec![Vec::with_capacity(frames); unit.outputs()];
    let mut done = 0;
    while done < frames {
        let n = BLOCK.min(frames - done);
        {
            let mut b = input.buffer_mut();
            for c in 0..unit.inputs() {
                for i in 0..BLOCK {
                    b.set_f32(c, i, if done + i == 0 { 1.0 } else { 0.0 });
                }
            }
        }
        unit.process(n, &input.buffer_ref(), &mut output.buffer_mut());
        let r = output.buffer_ref();
        for (c, ch) in out.iter_mut().enumerate() {
            ch.extend((0..n).map(|i| r.at_f32(c, i)));
        }
        done += n;
    }
    out
}

/// The latency `route` reports on **each** output.
///
/// `AudioUnit::latency()` is the *minimum* over outputs, so on a stereo node it
/// cannot see a wrong figure on one channel while the other is right — which is
/// exactly the shape a half-reverted `route` takes. PDC compensates per port.
fn output_latencies(unit: &mut dyn AudioUnit) -> Vec<Option<f64>> {
    let mut input = SignalFrame::new(unit.inputs());
    for i in 0..unit.inputs() {
        input.set(i, Signal::Latency(0.0));
    }
    let out = unit.route(&input, 1.0);
    (0..unit.outputs())
        .map(|o| match out.at(o) {
            Signal::Latency(l) => Some(l),
            _ => None,
        })
        .collect()
}

/// Frames at which `ch` carries energy.
fn onsets(ch: &[f32]) -> Vec<usize> {
    ch.iter()
        .enumerate()
        .filter(|(_, s)| s.abs() > 1e-4)
        .map(|(i, _)| i)
        .collect()
}

// ---------------------------------------------------------------------------
// D1: a musical delay reports zero latency.
// ---------------------------------------------------------------------------

/// A 500 ms echo is the effect, not processing latency: it must report 0, and
/// the dry half of the blend must leave at frame 0 to prove that 0 is true.
///
/// Mutation: restoring `input.at(0).delay(delay_samples)` in
/// `DelayLineNode::route` fails the `latency()` assertion (reports 24000).
/// Mutation: blending `mix.blend(delayed, delayed)` in `process_sample` (a node
/// whose whole output really *is* late) fails the frame-0 assertion.
#[test]
fn delay_line_reports_zero_latency_and_its_dry_path_is_immediate() {
    let mut node = DelayLineNode::new(1.0_f32, 0.5_f32, 0.0_f32);
    node.set_sample_rate(SR);
    node.set_mix(0.5_f32);

    assert_eq!(node.latency(), Some(0.0), "a musical delay is not latency");
    assert_eq!(output_latencies(&mut node), vec![Some(0.0)]);

    let echo = 24_000; // 0.5 s at 48 kHz
    let out = impulse_response(&mut node, echo + 16);
    assert_eq!(
        onsets(&out[0]),
        vec![0, echo],
        "dry at frame 0, echo at the delay time — the echo stays an echo"
    );
    assert!((out[0][0] - 0.5).abs() < 1e-6, "dry half is {}", out[0][0]);
}

/// Mutation: restoring `input.at(c).delay(d)` in
/// `StereoDelayLineNode::route` fails the `latency()` assertion.
#[test]
fn stereo_delay_line_reports_zero_latency_and_its_dry_path_is_immediate() {
    let mut node = StereoDelayLineNode::new(1.0_f32, 0.25_f32, 0.5_f32, 0.0_f32);
    node.set_sample_rate(SR);
    node.set_mix(0.5_f32);

    assert_eq!(node.latency(), Some(0.0), "a musical delay is not latency");
    assert_eq!(output_latencies(&mut node), vec![Some(0.0); 2]);

    let out = impulse_response(&mut node, 24_016);
    assert_eq!(
        onsets(&out[0]),
        vec![0, 12_000],
        "left: dry, then 250 ms echo"
    );
    assert_eq!(
        onsets(&out[1]),
        vec![0, 24_000],
        "right: dry, then 500 ms echo"
    );
}

/// Chorus and flanger share `ModulatedDelay`; their base delay is the sound.
///
/// Mutation: restoring `.delay(delay_samples)` in either `route` fails the
/// matching `latency()` assertion (about 480 samples for the chorus's 10 ms);
/// restoring it on one channel only fails the per-output assertion, which
/// `latency()`'s minimum cannot see.
/// Mutation: blending the delayed signal as the dry half in
/// `ModulatedDelay::process_sample` fails the frame-0 assertion.
#[test]
fn chorus_and_flanger_report_zero_latency_and_their_dry_path_is_immediate() {
    let mut chorus = ChorusNode::new();
    chorus.set_sample_rate(SR);
    chorus.set_mix(0.5_f32);
    let mut flanger = FlangerNode::new();
    flanger.set_sample_rate(SR);
    flanger.set_mix(0.5_f32);

    for (name, node) in [
        ("chorus", &mut chorus as &mut dyn AudioUnit),
        ("flanger", &mut flanger as &mut dyn AudioUnit),
    ] {
        assert_eq!(
            node.latency(),
            Some(0.0),
            "{name}: base delay is not latency"
        );
        assert_eq!(
            output_latencies(node),
            vec![Some(0.0); 2],
            "{name}: per output"
        );
        let out = impulse_response(node, 256);
        for (c, ch) in out.iter().enumerate() {
            assert!(
                (ch[0] - 0.5).abs() < 1e-6,
                "{name} ch{c}: frame 0 is {}, expected the dry half 0.5",
                ch[0]
            );
        }
    }
}

/// The consequence D1 was about, measured where it lands: PDC.
///
/// Three paths from the graph input — one through a 500 ms echo, one through a
/// chorus, one dry — must need no compensation at all. With the echo reporting
/// its delay time, `plan` delayed the dry output by 24 000 samples and the
/// chorus one by 24 000 − 480, i.e. PDC dragged the whole rest of the mix half
/// a second late to line up with an echo.
///
/// Mutation: restoring `.delay(delay_samples)` in `DelayLineNode::route` makes
/// `plan` non-empty (channel 2 needs 24000) and `compensate` splice delays.
#[test]
fn a_delay_insert_adds_no_compensation_to_the_other_paths() {
    let mut net = Net::new(1, 3);
    let mut echo = DelayLineNode::new(1.0_f32, 0.5_f32, 0.3_f32);
    echo.set_sample_rate(SR);
    echo.set_mix(0.5_f32);
    let mut chorus = ChorusNode::new();
    chorus.set_sample_rate(SR);
    let echo = net.add(echo);
    let chorus = net.add(chorus);
    let dry = net.add(Through::mono());
    net.set_source(echo, 0, Source::Global(0));
    net.set_source(chorus, 0, Source::Global(0));
    net.set_source(chorus, 1, Source::Global(0));
    net.set_source(dry, 0, Source::Global(0));
    net.set_output_source(0, Source::Local(echo, 0));
    net.set_output_source(1, Source::Local(chorus, 0));
    net.set_output_source(2, Source::Local(dry, 0));

    let plan = latency::plan(&net);
    assert!(plan.is_empty(), "compensation {:?}", plan.channels());

    let before = net.size();
    let applied = latency::compensate(&mut net);
    assert!(applied.is_empty());
    assert_eq!(
        net.size(),
        before,
        "compensate spliced a delay into the graph"
    );
}

/// A summing bus does not hide the latency of what feeds it.
///
/// `ChannelSumNode` replaced fundsp's `sum` as the engine's fan-in, and its
/// `route` answered `Latency(0)` whatever arrived. A lookahead limiter summed
/// with a dry path then read as a zero-latency graph, and `reported_latency`
/// (which is this `latency()`) pre-rolled an export by nothing. The bus now
/// carries its latest input, so the graph reports the limiter's lookahead.
///
/// Mutation: reverting the bus's `route` to `Latency(0)` fails the first
/// assertion; taking the `min` of its inputs fails it too (the dry path is 0).
#[test]
fn a_limiter_summed_with_a_dry_path_reports_the_limiter_latency() {
    use tutti_core::{ChannelLayout, Db};
    use tutti_nodes::{ChannelSumNode, LimiterNode};

    let mut net = Net::new(1, 1);
    let lim = net.add(LimiterNode::with_channels(
        ChannelLayout::MONO,
        Db(-1.0),
        Db(-0.3),
    ));
    let dry = net.add(Through::mono());
    let sum = net.add(ChannelSumNode::new(2, ChannelLayout::MONO));
    net.set_source(lim, 0, Source::Global(0));
    net.set_source(dry, 0, Source::Global(0));
    net.set_source(sum, 0, Source::Local(lim, 0));
    net.set_source(sum, 1, Source::Local(dry, 0));
    net.set_output_source(0, Source::Local(sum, 0));
    net.set_sample_rate(SR);

    let lookahead = net.node_mut(lim).latency().expect("the limiter reports");
    assert!(lookahead > 0.0, "a lookahead limiter has latency");
    assert_eq!(net.latency(), Some(lookahead));
}

// ---------------------------------------------------------------------------
// D3: the convolver's reported latency is true of the whole output.
// ---------------------------------------------------------------------------
//
// Behind the crate's `convolution` feature, as the convolver is. A workspace
// run turns it on (tutti-export depends on it); for this crate alone, pass
// `--features convolution`.
#[cfg(feature = "convolution")]
mod convolver {
    use super::*;
    use tutti_core::Samples;
    use tutti_nodes::{ConvolverNode, StereoConvolverNode};

    /// With a unit-impulse IR the wet path is a pure delay of the reported latency.
    /// So at *every* mix the output must be one impulse, at exactly that latency,
    /// with amplitude `dry_share + wet_share` = 1 — dry and wet land on the same
    /// frame. Before the fix, `mix < 1` put the dry share at frame 0.
    ///
    /// Mutation: blending `mix.blend(input, wet)` (the undelayed input) again fails
    /// at mix 0.5 (two onsets) and mix 0.0 (onset at 0, not the latency).
    /// Mutation: sizing `DryAlign` at `latency + 1` fails every mix below 1.
    #[test]
    fn convolver_dry_and_wet_leave_together_at_the_reported_latency() {
        for mix in [0.0_f32, 0.5, 1.0] {
            let mut node = ConvolverNode::new(&[1.0], 256);
            node.set_sample_rate(SR);
            node.set_mix(mix);

            let latency = node.latency_samples().0;
            assert_eq!(latency, 256);
            assert_eq!(node.latency(), Some(latency as f64), "mix {mix}");

            let out = impulse_response(&mut node, latency * 3);
            assert_eq!(
                onsets(&out[0]),
                vec![latency],
                "mix {mix}: one aligned impulse"
            );
            assert!(
                (out[0][latency] - 1.0).abs() < 1e-5,
                "mix {mix}: dry + wet sum to {}, expected 1",
                out[0][latency]
            );
        }
    }

    /// The stereo node in all three channel configs: each channel's dry input is
    /// delayed by the same latency as its wet path.
    ///
    /// Mutation: replacing `self.dry.r.step(in_r)` with the undelayed `in_r`
    /// fails on ch1 (two onsets at mix 0.5), which the per-channel loop names.
    #[test]
    fn stereo_convolver_dry_and_wet_leave_together_at_the_reported_latency() {
        type Build = fn() -> StereoConvolverNode;
        let builds: [(&str, Build); 3] = [
            ("mono", || StereoConvolverNode::mono(&[1.0], 128)),
            ("stereo", || {
                StereoConvolverNode::stereo(&[1.0], &[1.0], 128)
            }),
            ("mono_to_stereo", || {
                StereoConvolverNode::mono_to_stereo(&[1.0], &[1.0], 128)
            }),
        ];
        for (name, build) in builds {
            for mix in [0.0_f32, 0.5, 1.0] {
                let mut node = build();
                node.set_sample_rate(SR);
                node.set_mix(mix);
                let latency = node.latency_samples().0;
                assert_eq!(node.latency(), Some(latency as f64), "{name} mix {mix}");
                assert_eq!(output_latencies(&mut node), vec![Some(latency as f64); 2]);

                let out = impulse_response(&mut node, latency * 3);
                for (c, ch) in out.iter().enumerate() {
                    assert_eq!(
                        onsets(ch),
                        vec![latency],
                        "{name} mix {mix} ch{c}: dry and wet must share one frame"
                    );
                }
            }
        }
    }

    /// `reset` must clear the dry ring along with the convolver, or the last take's
    /// dry input replays for one latency after a discontinuity.
    ///
    /// Mutation: removing `self.dry.clear()` from `ConvolverNode::reset` fails.
    #[test]
    fn convolver_reset_clears_the_dry_alignment_ring() {
        let mut node = ConvolverNode::new(&[1.0], 64);
        node.set_sample_rate(SR);
        node.set_mix(0.0_f32);

        // Load the dry ring with a take, then reset mid-ring.
        let mut out = [0.0f32];
        for _ in 0..32 {
            node.tick(&[1.0], &mut out);
        }
        node.reset();

        let mut peak = 0.0f32;
        for _ in 0..256 {
            node.tick(&[0.0], &mut out);
            peak = peak.max(out[0].abs());
        }
        assert!(
            peak < 1e-6,
            "reset left {peak} of the old dry take in the ring"
        );
    }

    /// Real processing latency is still compensated, and a musical delay
    /// downstream of it adds nothing on top: convolver → echo on channel 0, the
    /// echo alone on channel 1, dry on channel 2. Only the convolver's block is
    /// latency, so channels 1 and 2 each pre-roll by exactly that block.
    ///
    /// This is the half of the property the zero-latency test above cannot
    /// show — that the fix did not just switch PDC off.
    ///
    /// Mutation: restoring `.delay(delay_samples)` in `DelayLineNode::route`
    /// fails (`[0, 256, 24256]`). Mutation: making the convolver's `route`
    /// report no delay fails (an empty plan).
    #[test]
    fn a_delay_after_a_convolver_adds_nothing_to_its_compensation() {
        let mut net = Net::new(1, 3);
        let mut conv = ConvolverNode::new(&[1.0], 256);
        conv.set_sample_rate(SR);
        let latency = conv.latency_samples();
        let mut echo_a = DelayLineNode::new(1.0_f32, 0.5_f32, 0.0_f32);
        echo_a.set_sample_rate(SR);
        let mut echo_b = DelayLineNode::new(1.0_f32, 0.5_f32, 0.0_f32);
        echo_b.set_sample_rate(SR);

        let conv = net.add(conv);
        let echo_a = net.add(echo_a);
        let echo_b = net.add(echo_b);
        let dry = net.add(Through::mono());
        net.set_source(conv, 0, Source::Global(0));
        net.set_source(echo_a, 0, Source::Local(conv, 0));
        net.set_source(echo_b, 0, Source::Global(0));
        net.set_source(dry, 0, Source::Global(0));
        net.set_output_source(0, Source::Local(echo_a, 0));
        net.set_output_source(1, Source::Local(echo_b, 0));
        net.set_output_source(2, Source::Local(dry, 0));

        let plan = latency::plan(&net);
        assert_eq!(plan.total(), latency);
        assert_eq!(plan.channels(), &[Samples(0), latency, latency]);
    }
}
