//! **A node's blend with its input is the graph's mix**, not the node's: the
//! convolver, the delay, the chorus and flanger, the phaser and the two reverbs output their wet
//! signal alone, and `Editor::set_mix` blends it with the input delayed by the
//! node's declared latency (doc 013, "Bypass and mute are graph state", M4).
//! These nodes once held a dry/wet of their own; each property they pinned is
//! here, through the graph:
//!
//! - at `Mix::WET` the graph's output is the node rendered alone, bit for bit;
//! - at `Mix::DRY` the output is the input delayed by the node's latency, bit
//!   for bit (the convolver's block; none for the rest), on the channels the
//!   input has (a reverb's mono input leaves its right channel silent: the graph's
//!   rule is `y * m` where there is no `x̂`);
//! - at a mix between, the output is `y * m + x̂ * (1 - m)`, with `y` the node's
//!   own output (rendered alone, at `WET`) and `x̂` the aligned input, bit for bit;
//! - a mix that moves ramps across the block it lands on, to the new mix.
//!
//! Each render is preceded by one block of silence, the block the mix's ramp from
//! the unit's `WET` start lands on, and only the frames after it are compared.

use tutti_dsp::{FdnControls, FdnDesign, FreeverbControls, FreeverbDesign};
use tutti_fixtures::noise;
use tutti_graph::contract::drive;
use tutti_graph::{IntoNode, Prepare, Solo};
use tutti_nodes::{
    CompressorNode, CrossoverNode, DelayLineNode, DesignNode, GateNode, ModDelayNode, PhaserNode,
};
use tutti_types::{
    ChannelLayout, CompressionRatio, Db, Feedback, Hz, Mix, SampleRate, Samples, Seconds,
};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;
const FRAMES: usize = 1_024;

fn solo<N: IntoNode>(node: N) -> Solo<N::Controls> {
    Solo::new(node, Prepare::new(RATE, Samples(BLOCK)))
}

fn set_mix<C>(solo: &mut Solo<C>, mix: Mix) {
    let key = solo.key();
    solo.renderer_mut()
        .editor_mut()
        .set_mix(key, mix)
        .expect("the node is in the graph and the mix is in range");
}

/// `BLOCK` frames of silence, then `x`, on every channel.
fn after_silence(x: &[Vec<f32>]) -> Vec<Vec<f32>> {
    x.iter()
        .map(|c| {
            let mut v = vec![0.0; BLOCK];
            v.extend_from_slice(c);
            v
        })
        .collect()
}

/// What `make()` renders for `x` (after a silent block) at `mix` in a graph,
/// the silent block cut off.
fn rendered<N: IntoNode>(make: &dyn Fn() -> N, x: &[Vec<f32>], mix: Mix) -> Vec<Vec<f32>> {
    let mut s = solo(make());
    set_mix(&mut s, mix);
    let input = after_silence(x);
    let refs: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
    s.render_input(&refs)
        .into_iter()
        .map(|c| c[BLOCK..].to_vec())
        .collect()
}

fn bits(x: &[f32]) -> Vec<u32> {
    x.iter().map(|v| v.to_bits()).collect()
}

/// The input delayed by `latency` frames: what the graph's `x̂` is.
fn delayed(x: &[f32], latency: usize) -> Vec<f32> {
    let mut v = vec![0.0; latency];
    v.extend_from_slice(&x[..x.len() - latency]);
    v
}

/// The three properties above for the node `make` builds, of `latency` frames
/// declared and `width` channels in and out.
fn check<N: IntoNode>(name: &str, make: &dyn Fn() -> N, width: usize, latency: usize) {
    check_io(name, make, width, width, latency);
}

/// [`check`] for a node of `inputs` channels in and `outputs` out, whose main
/// buses are placed (a design's `Placement::Speakers`): a mono input is a centre,
/// which the graph's fold sends to every side of the output at gain 1.0, so each
/// output's `x̂` is that input; otherwise output `c`'s `x̂` is input `c`.
fn check_io<N: IntoNode>(
    name: &str,
    make: &dyn Fn() -> N,
    inputs: usize,
    outputs: usize,
    latency: usize,
) {
    let x: Vec<Vec<f32>> = (0..inputs).map(|c| noise(40 + c as u32, FRAMES)).collect();
    let aligned = |c: usize| match x.get(if inputs == 1 { 0 } else { c }) {
        Some(x) => delayed(x, latency),
        None => vec![0.0; FRAMES],
    };

    // DRY is the aligned input.
    let dry = rendered(make, &x, Mix::DRY);
    for (c, dry) in dry.iter().enumerate().take(outputs) {
        assert_eq!(
            bits(dry),
            bits(&aligned(c)),
            "{name}: channel {c} at DRY is the input delayed by {latency}"
        );
    }

    // WET is the node alone.
    let wet = rendered(make, &x, Mix::WET);
    let (mut alone, _) = make().into_node();
    alone = {
        alone.prepare(&Prepare::new(RATE, Samples(BLOCK)));
        alone
    };
    let input = after_silence(&x);
    let mut solo_out: Vec<Vec<f32>> = vec![Vec::new(); outputs];
    for at in (0..input[0].len()).step_by(BLOCK) {
        let block: Vec<&[f32]> = input.iter().map(|c| &c[at..at + BLOCK]).collect();
        for (o, b) in solo_out
            .iter_mut()
            .zip(drive(alone.as_mut(), RATE, &block, &[]))
        {
            o.extend(b);
        }
    }
    for c in 0..outputs {
        assert_eq!(
            bits(&wet[c]),
            bits(&solo_out[c][BLOCK..]),
            "{name}: channel {c} at WET is the node alone"
        );
    }

    // A mix between is the formula.
    let m = 0.3_f32;
    let mixed = rendered(make, &x, Mix::new(m));
    for c in 0..outputs {
        let want: Vec<f32> = if c < inputs || inputs == 1 {
            wet[c]
                .iter()
                .zip(aligned(c))
                .map(|(&y, x)| y * m + x * (1.0 - m))
                .collect()
        } else {
            wet[c].iter().map(|&y| y * m).collect()
        };
        assert_eq!(
            bits(&mixed[c]),
            bits(&want),
            "{name}: channel {c} at {m} is y*m + x*(1-m)"
        );
    }
}

/// Mutations (run, each failed): `DelayLineNode::shape` declaring a latency of
/// one frame it does not have (the graph's aligned input one frame late) fails
/// the delay's DRY comparison, its formula and its ramp test; the chorus
/// declaring 480 frames fails the chorus and flanger check; the phaser declaring
/// one frame fails the phaser check; the convolver declaring its block plus one
/// or none fails its checks.
#[cfg(feature = "convolution")]
#[test]
fn the_convolver_is_blended_by_the_graph() {
    use tutti_nodes::{ConvolverNode, generate_room_ir};
    let ir = generate_room_ir(0.5, Seconds(0.05), RATE);
    let ir2 = generate_room_ir(0.8, Seconds(0.08), RATE);
    check(
        "convolver (stereo, 64-frame block)",
        &|| ConvolverNode::stereo(&ir, &ir2, 64),
        2,
        64,
    );
    check(
        "convolver (shared IR, six channels)",
        &|| ConvolverNode::shared_ir(6u16, &[1.0, 0.5], 128),
        6,
        128,
    );
    check(
        "convolver (folded, stereo)",
        &|| ConvolverNode::mono_to_stereo(&ir, &ir2, 32),
        2,
        32,
    );
}

#[test]
fn the_delay_is_blended_by_the_graph() {
    check(
        "delay (stereo, cross-fed)",
        &|| {
            let d = DelayLineNode::stereo(
                Seconds(0.1),
                Seconds(0.013),
                Seconds(0.021),
                Feedback::new(0.5),
            );
            d.set_cross_feedback(Feedback::new(0.3));
            d
        },
        2,
        0,
    );
    check(
        "delay (mono)",
        &|| DelayLineNode::new(Seconds(0.1), Seconds(0.01), Feedback::new(0.4)),
        1,
        0,
    );
}

#[test]
fn the_chorus_flanger_and_phaser_are_blended_by_the_graph() {
    check(
        "chorus (stereo)",
        &|| ModDelayNode::chorus(ChannelLayout::STEREO),
        2,
        0,
    );
    check(
        "flanger (mono)",
        &|| ModDelayNode::flanger(ChannelLayout::MONO),
        1,
        0,
    );
    check(
        "phaser (stereo, 4 stages)",
        &|| PhaserNode::with_channels(ChannelLayout::STEREO, 4),
        2,
        0,
    );
}

/// The Freeverb and the feedback delay network, mono in and stereo out, are
/// blended by the graph like the rest: WET is the node alone, DRY the aligned
/// input on **both** sides (a mono source is a centre, which the fold sends to
/// left and right at gain 1.0: the design places its channels,
/// `Placement::Speakers`), and between them `y * m + x̂ * (1 - m)`, bit for bit.
///
/// Mutations (run): every reverb's shape declaring one frame of latency it does
/// not have (the graph's aligned input one frame late) fails the DRY comparison
/// and the formula; the reverbs' main buses left as `Shape::audio` builds them (mono
/// into stereo naming no placement) → the graph refuses their mix
/// (`MixError::Unplaced`) and the test fails at its first `set_mix`. (A residual dry or a scaled wet in the node cannot show here,
/// where WET is the node compared with itself: `reverb_node.rs`'s
/// `the_node_is_the_room_alone` pins the analytic wet.)
#[test]
fn the_reverbs_are_blended_by_the_graph() {
    check_io(
        "freeverb (General MIDI room)",
        &|| DesignNode::fixed(FreeverbDesign, FreeverbControls::GENERAL_MIDI),
        1,
        2,
        0,
    );
    check_io(
        "freeverb (live)",
        &|| DesignNode::live(FreeverbDesign, FreeverbControls::GENERAL_MIDI),
        1,
        2,
        0,
    );
    check_io(
        "fdn",
        &|| DesignNode::live(FdnDesign::default(), FdnControls::DEFAULT),
        1,
        2,
        0,
    );
}

/// A mix that moves ramps across the block it lands on, from where it was to the
/// new mix: moving the delay's (six wide) from `WET` to `DRY` leaves the block
/// `y * m_i + x * (1 - m_i)` with `m_i = 1 - (i + 1) / 64`, ending on the dry
/// input exactly. (The delay node's own mix once did this: its
/// `a_mix_change_fades_across_the_next_block`.)
///
/// Mutation (run): the delay declaring one frame of latency fails it, like the
/// other delay tests here.
#[test]
fn a_mix_move_on_a_delay_ramps_across_the_block() {
    let make =
        || DelayLineNode::with_channels(6u16, Seconds(0.1), Seconds(0.0005), Feedback::new(0.3));
    let x: Vec<Vec<f32>> = (0..6).map(|c| noise(70 + c, 2 * BLOCK)).collect();
    let wet = {
        let mut s = solo(make());
        let refs: Vec<&[f32]> = x.iter().map(Vec::as_slice).collect();
        s.render_input(&refs)
    };
    let mut s = solo(make());
    let first: Vec<&[f32]> = x.iter().map(|c| &c[..BLOCK]).collect();
    s.render_input(&first);
    set_mix(&mut s, Mix::DRY);
    let second: Vec<&[f32]> = x.iter().map(|c| &c[BLOCK..]).collect();
    let got = s.render_input(&second);
    for c in 0..6 {
        for i in 0..BLOCK {
            let m = 1.0 + (0.0 - 1.0) * ((i + 1) as f32 / BLOCK as f32);
            let want = wet[c][BLOCK + i] * m + x[c][BLOCK + i] * (1.0 - m);
            assert_eq!(
                got[c][i].to_bits(),
                want.to_bits(),
                "channel {c} frame {i}: m = {m}"
            );
        }
        assert_eq!(
            got[c][BLOCK - 1].to_bits(),
            x[c][2 * BLOCK - 1].to_bits(),
            "channel {c}: the block ends fully dry"
        );
    }
}

/// **A keyed node and a split are blended on their main bus.** The gate and the
/// compressor take their audio, then a key as wide (a `Sidechain` bus): the dry
/// side of their mix is the audio, never the key. The crossover writes its
/// input's channels once per band, band-major: the dry side is band 0, the
/// further bands silent. Each declares it; built from widths alone, each would
/// have no pass and the graph would refuse its mix.
///
/// Mutation (each run, each failed): the gate's or the compressor's
/// `with_buses_in(keyed(..))` / `with_buses_out(..)` removed → `set_mix`
/// refuses (`MixError::Unplaced`, four in and two out naming no placement);
/// the crossover's band 0 declared at the full output width
/// (`Placement::Bands => self.channels.output`) → the mono crossover's centre
/// folds onto both bands and band 1 at DRY is the input. (The stereo one cannot
/// show that mutation: four channels are quad, and a stereo pair folds onto its
/// front pair, band 0, either way.)
#[test]
fn a_keyed_node_and_a_split_are_blended_on_their_main_bus() {
    let s = Seconds::new;
    check_io(
        "gate (stereo, keyed)",
        &|| GateNode::stereo(Db(-20.0), s(0.001), s(0.01), s(0.05)),
        4,
        2,
        0,
    );
    check_io(
        "compressor (stereo, keyed)",
        &|| CompressorNode::stereo(Db(-20.0), CompressionRatio::new(4.0), s(0.005), s(0.05)),
        4,
        2,
        0,
    );
    check_io(
        "crossover (stereo, two bands)",
        &|| CrossoverNode::new(ChannelLayout::STEREO, &[Hz(1_000.0)]).expect("one split"),
        2,
        4,
        0,
    );
    // Mono: band 0 is the input at DRY, band 1 silent (not a centre on both).
    let x = vec![noise(90, FRAMES)];
    let dry = rendered(
        &|| CrossoverNode::new(ChannelLayout::MONO, &[Hz(1_000.0)]).expect("one split"),
        &x,
        Mix::DRY,
    );
    assert_eq!(bits(&dry[0]), bits(&x[0]), "mono crossover: band 0 at DRY");
    assert!(
        dry[1].iter().all(|&v| v == 0.0),
        "mono crossover: band 1 at DRY is silent"
    );
}
