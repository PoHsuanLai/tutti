//! A node's declared latency (`Shape::latency`) must be the delay the output
//! actually has.
//!
//! PDC reads nothing but that figure: it delays every *other* path by it. So a
//! figure that is too high (a musical delay reported as latency) drags the
//! rest of the mix late, and a figure that is true of only part of the output
//! (the convolver's wet half) leaves the other part early after
//! compensation. Each test therefore pins the reported number **and** measures
//! the output with an impulse, because the number alone cannot fail on a node
//! whose DSP drifted away from it.

use tutti_graph::contract::{drive, prepared};
use tutti_graph::{GraphBuilder, IntoNode, Node, Prepare};
use tutti_nodes::testing::Through;
use tutti_nodes::{CrossoverNode, DelayLineNode, ModDelayNode, MultibandCompressorNode};
use tutti_types::Latency;
use tutti_types::MaxBlock;
use tutti_types::{ChannelLayout, Feedback, Hz, Mix, Seconds};
use tutti_types::{SampleRate, Samples};

const SR: SampleRate = SampleRate(48_000.0);
/// The block the impulse responses are rendered in.
const BLOCK: usize = 64;

/// Run `node` (prepared here) over `frames` frames of a unit impulse at frame
/// 0 on every input, through `process` in [`BLOCK`]-frame blocks. Returns one
/// `Vec` per output.
fn impulse_response<N: Node>(node: N, frames: usize) -> Vec<Vec<f32>> {
    let mut node = prepared(node, SR, BLOCK);
    let ins = usize::from(node.shape().audio_in().count());
    let mut out: Vec<Vec<f32>> = Vec::new();
    let mut done = 0;
    while done < frames {
        let n = BLOCK.min(frames - done);
        let chunk: Vec<f32> = (done..done + n)
            .map(|i| if i == 0 { 1.0 } else { 0.0 })
            .collect();
        let inputs: Vec<&[f32]> = (0..ins).map(|_| &chunk[..]).collect();
        let block = drive(&mut node, SR, &inputs, &[]);
        out.resize(block.len(), Vec::with_capacity(frames));
        for (ch, b) in out.iter_mut().zip(block) {
            ch.extend(b);
        }
        done += n;
    }
    out
}

/// `node` alone in a graph, blended with its aligned input at `mix` (the graph's
/// `set_mix`, set before the unit is placed so no block ramps in from wet), and
/// run over `frames` frames of a unit impulse at frame 0 on every input. One
/// `Vec` per output.
fn mixed_impulse_response<N: IntoNode>(node: N, frames: usize, mix: Mix) -> Vec<Vec<f32>> {
    let (node, _) = node.into_node();
    let shape = node.shape();
    let mut g = GraphBuilder::new(shape.audio_in(), shape.audio_out());
    let key = g.add(tutti_graph::Unforkable(node));
    for c in 0..usize::from(shape.audio_in().count()) {
        g.connect_input(c, key, c);
    }
    for c in 0..usize::from(shape.audio_out().count()) {
        g.connect_output(key, c, c);
    }
    g.spec_mut().set_mix(key, mix);
    let mut r = g
        .renderer(Prepare::new(SR, MaxBlock::of(Samples(BLOCK))))
        .expect("a one-node graph builds");
    let mut chunk = vec![0.0f32; frames];
    chunk[0] = 1.0;
    let ins: Vec<&[f32]> = (0..usize::from(shape.audio_in().count()))
        .map(|_| &chunk[..])
        .collect();
    r.render_input(&ins)
}

/// The latency a node declares: one figure for its whole output (a graph
/// node has no per-output latency to get half wrong).
fn declared(node: &dyn Node) -> Latency {
    node.shape().latency
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
// An IIR crossover reports zero latency.
// ---------------------------------------------------------------------------

/// The Linkwitz-Riley split is IIR and not linear-phase: it is not delayed,
/// so it declares none, and every band answers an impulse on the impulse's
/// own frame. A low band's first sample is tiny (a 300 Hz low-pass has
/// `b0^4` of order 1e-14) but never zero, so the check is on exactly that;
/// the summed output answers well above zero.
#[test]
fn crossover_and_multiband_report_zero_latency_and_respond_at_frame_0() {
    let xo = CrossoverNode::new(ChannelLayout::MONO, &[Hz(300.0), Hz(3_000.0)]).unwrap();
    assert_eq!(declared(&xo), Latency::ZERO);
    for (b, band) in impulse_response(xo, 64).iter().enumerate() {
        assert!(band[0] > 0.0, "band {b} is silent at frame 0");
    }

    let mb = MultibandCompressorNode::new(ChannelLayout::MONO, 4).unwrap();
    assert_eq!(declared(&mb), Latency::ZERO);
    let out = impulse_response(mb, 64);
    assert_eq!(onsets(&out[0]).first(), Some(&0));
    assert!(out[0][0] > 0.1, "the sum answers at frame 0: {}", out[0][0]);
}

// ---------------------------------------------------------------------------
// A musical delay reports zero latency.
// ---------------------------------------------------------------------------

/// A 500 ms echo is the effect, not processing latency: it must declare 0,
/// and the dry half of the graph's mix at 0.5 must leave at frame 0 to prove
/// that 0 is true.
#[test]
fn delay_line_reports_zero_latency_and_its_dry_path_is_immediate() {
    let node = DelayLineNode::new(
        Seconds::new(1.0_f64),
        Seconds::new(0.5_f64),
        Feedback::new(0.0_f32),
    );

    assert_eq!(
        declared(&node),
        Latency::ZERO,
        "a musical delay is not latency"
    );

    let echo = 24_000; // 0.5 s at 48 kHz
    let out = mixed_impulse_response(node, echo + 16, Mix::new(0.5_f32));
    assert_eq!(
        onsets(&out[0]),
        vec![0, echo],
        "dry at frame 0, echo at the delay time — the echo stays an echo"
    );
    assert!((out[0][0] - 0.5).abs() < 1e-6, "dry half is {}", out[0][0]);
}

/// The stereo delay: each channel's dry half leaves at frame 0 and its echo
/// at its own time.
#[test]
fn stereo_delay_line_reports_zero_latency_and_its_dry_path_is_immediate() {
    let node = DelayLineNode::stereo(
        Seconds::new(1.0_f64),
        Seconds::new(0.25_f64),
        Seconds::new(0.5_f64),
        Feedback::new(0.0_f32),
    );

    assert_eq!(
        declared(&node),
        Latency::ZERO,
        "a musical delay is not latency"
    );

    let out = mixed_impulse_response(node, 24_016, Mix::new(0.5_f32));
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

/// Chorus and flanger are one `ModDelayNode`; their base delay is the sound.
#[test]
fn chorus_and_flanger_report_zero_latency_and_their_dry_path_is_immediate() {
    for (name, node) in [
        ("chorus", ModDelayNode::chorus(ChannelLayout::STEREO)),
        ("flanger", ModDelayNode::flanger(ChannelLayout::STEREO)),
    ] {
        assert_eq!(
            declared(&node),
            Latency::ZERO,
            "{name}: base delay is not latency"
        );
        let out = mixed_impulse_response(node, 256, Mix::new(0.5_f32));
        for (c, ch) in out.iter().enumerate() {
            assert!(
                (ch[0] - 0.5).abs() < 1e-6,
                "{name} ch{c}: frame 0 is {}, expected the dry half 0.5",
                ch[0]
            );
        }
    }
}

/// The consequence of a delay reporting latency, measured where it lands:
/// the graph's PDC.
///
/// Three paths from the graph input — one through a 500 ms echo, one through
/// a chorus, one dry — must need no compensation at all: no output pre-rolls
/// and the compiler inserts no delay. Were the echo to declare its delay
/// time, the compiler would delay the dry output by 24 000 samples and the
/// chorus one by 24 000, i.e. PDC would drag the whole rest of the mix half a
/// second late to line up with an echo.
#[test]
fn a_delay_insert_adds_no_compensation_to_the_other_paths() {
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::from_count(3));
    let echo = DelayLineNode::new(
        Seconds::new(1.0_f64),
        Seconds::new(0.5_f64),
        Feedback::new(0.3_f32),
    );
    let (echo, _) = g.add_with_controls(echo);
    g.spec_mut().set_mix(echo, Mix::new(0.5_f32));
    let (chorus, _) = g.add_with_controls(ModDelayNode::chorus(ChannelLayout::STEREO));
    let dry = g.add(Through::mono());
    g.connect_input(0, echo, 0)
        .connect_input(0, chorus, 0)
        .connect_input(0, chorus, 1)
        .connect_input(0, dry, 0)
        .connect_output(echo, 0, 0)
        .connect_output(chorus, 0, 1)
        .connect_output(dry, 0, 2);
    let (_editor, exec) = g
        .build(Prepare::new(SR, MaxBlock::of(Samples(BLOCK))))
        .expect("the graph builds");
    let plan = exec.plan().expect("committed");
    assert_eq!(plan.compensation(), &[Samples(0); 3], "no output pre-rolls");
    assert!(plan.delays().is_empty(), "the compiler spliced a delay");
}

/// A summing bus does not hide the latency of what feeds it.
///
/// The graph's fan-in is the engine's sum: output channel 0 holds two sources,
/// the limiter and the dry input. Were the sum to hide its sources' latency, a
/// lookahead limiter summed with a dry path would read as a zero-latency
/// graph, and an export would pre-roll by nothing. The compiler's PDC aligns
/// each source of the sum: the graph reports the limiter's lookahead, and the
/// dry source is delayed to meet it, so the impulse leaves **once**, at
/// exactly that frame.
#[test]
fn a_limiter_summed_with_a_dry_path_reports_the_limiter_latency() {
    use tutti_graph::Source;
    use tutti_graph::{GraphBuilder, Prepare};
    use tutti_nodes::LimiterNode;
    use tutti_types::Latency;
    use tutti_types::{ChannelLayout, Db, Samples, Seconds};

    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let (lim, _) = g.add_with_controls(LimiterNode::with_channels(
        ChannelLayout::MONO,
        Db(-1.0),
        Db(-0.3),
    ));
    g.connect_input(0, lim, 0);
    g.connect_output(lim, 0, 0);
    g.add_output(0, Source::Global(0));
    let mut r = g
        .renderer(Prepare::new(SR, MaxBlock::of(Samples(BLOCK))))
        .expect("builds");

    let lookahead = Seconds(0.005).to_samples_ceil(SR);
    assert_eq!(
        r.executor().plan().expect("committed").total_latency(),
        Latency::new(lookahead),
        "the graph reports the limiter's lookahead through the bus"
    );
    let mut impulse = vec![0.0f32; 512];
    impulse[0] = 0.25;
    let out = r.render_input(&[&impulse]).remove(0);
    assert_eq!(
        onsets(&out),
        vec![lookahead.get()],
        "the dry path is compensated to meet the limiter"
    );
}

// ---------------------------------------------------------------------------
// The convolver's reported latency is true of the whole output.
// ---------------------------------------------------------------------------
//
// Behind the crate's `convolution` feature, as the convolver is. A workspace
// run turns it on (tutti-export depends on it); for this crate alone, pass
// `--features convolution`.
#[cfg(feature = "convolution")]
mod convolver {
    use super::*;
    use tutti_nodes::ConvolverNode;

    /// With a unit-impulse IR the wet path is a pure delay of the reported latency.
    /// So at *every* graph mix the output must be one impulse, at exactly that
    /// latency, with amplitude `dry_share + wet_share` = 1 — dry (the input
    /// delayed by the declared latency) and wet land on the same frame.
    #[test]
    fn convolver_dry_and_wet_leave_together_at_the_reported_latency() {
        for mix in [0.0_f32, 0.5, 1.0] {
            let node = ConvolverNode::new(&[1.0], 256);

            let latency = node.latency_samples().0;
            assert_eq!(latency, 256);
            assert_eq!(declared(&node), Latency::new(Samples(latency)), "mix {mix}");

            let out = mixed_impulse_response(node, latency * 3, Mix::new(mix));
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

    /// The wide node in all three channel configs, at width 2 and width 6:
    /// each channel's dry input (the graph's aligned input) is delayed by the
    /// same latency as its wet path.
    #[test]
    fn stereo_convolver_dry_and_wet_leave_together_at_the_reported_latency() {
        type Build = fn() -> ConvolverNode;
        let builds: [(&str, Build); 6] = [
            ("mono", || ConvolverNode::shared_ir(2u16, &[1.0], 128)),
            ("stereo", || ConvolverNode::stereo(&[1.0], &[1.0], 128)),
            ("mono_to_stereo", || {
                ConvolverNode::mono_to_stereo(&[1.0], &[1.0], 128)
            }),
            ("shared x6", || ConvolverNode::shared_ir(6u16, &[1.0], 128)),
            ("per_channel x6", || {
                ConvolverNode::per_channel(&[&[1.0_f32][..]; 6], 128)
            }),
            ("folded x6", || {
                ConvolverNode::folded(&[&[1.0_f32][..]; 6], 128)
            }),
        ];
        for (name, build) in builds {
            for mix in [0.0_f32, 0.5, 1.0] {
                let node = build();
                let latency = node.latency_samples().0;
                assert_eq!(
                    declared(&node),
                    Latency::new(Samples(latency)),
                    "{name} mix {mix}"
                );

                let out = mixed_impulse_response(node, latency * 3, Mix::new(mix));
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

    /// `reset` must clear the convolver's buffered input, or the last take replays
    /// for one latency after a discontinuity. (The dry half is the graph's: its
    /// aligned input is cleared by the graph's own reset.)
    #[test]
    fn convolver_reset_clears_the_buffered_take() {
        let mut node = prepared(ConvolverNode::new(&[1.0], 64), SR, 256);

        // Load the convolver with a take, then reset mid-block.
        drive(&mut node, SR, &[&[1.0; 32]], &[]);
        Node::reset(&mut node);

        let out = drive(&mut node, SR, &[&[0.0; 256]], &[]);
        let peak = out[0].iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(
            peak < 1e-6,
            "reset left {peak} of the previous take in the convolver"
        );
    }

    /// Real processing latency is still compensated, and a musical delay
    /// downstream of it adds nothing on top: convolver → echo on channel 0, the
    /// echo alone on channel 1, dry on channel 2. Only the convolver's block is
    /// latency, so channels 1 and 2 each pre-roll by exactly that block.
    ///
    /// This is the half of the property the zero-latency test above cannot
    /// show — that the fix did not just switch PDC off.
    #[test]
    fn a_delay_after_a_convolver_adds_nothing_to_its_compensation() {
        let conv = ConvolverNode::new(&[1.0], 256);
        let latency = conv.latency_samples();
        let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::from_count(3));
        let (conv, _) = g.add_with_controls(conv);
        let (echo_a, _) = g.add_with_controls(DelayLineNode::new(
            Seconds::new(1.0_f64),
            Seconds::new(0.5_f64),
            Feedback::new(0.0_f32),
        ));
        let (echo_b, _) = g.add_with_controls(DelayLineNode::new(
            Seconds::new(1.0_f64),
            Seconds::new(0.5_f64),
            Feedback::new(0.0_f32),
        ));
        let dry = g.add(Through::mono());
        g.connect_input(0, conv, 0)
            .connect(conv, 0, echo_a, 0)
            .connect_input(0, echo_b, 0)
            .connect_input(0, dry, 0)
            .connect_output(echo_a, 0, 0)
            .connect_output(echo_b, 0, 1)
            .connect_output(dry, 0, 2);
        let (_editor, exec) = g
            .build(Prepare::new(SR, MaxBlock::of(Samples(BLOCK))))
            .expect("the graph builds");
        let plan = exec.plan().expect("committed");
        assert_eq!(plan.total_latency(), Latency::new(latency));
        assert_eq!(plan.compensation(), &[Samples(0), latency, latency]);
    }
}
