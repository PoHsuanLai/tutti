//! The filter nodes' steady-state magnitude responses, against the closed forms
//! of what each filter is (doc 024): a sine is rendered through the node and the
//! gain read from the settled half. The SVF low-pass and high-pass are held to an
//! independent biquad in `oracle_biquad.rs`; what moves cells and lanes inside a
//! block is held by `filter_block_independence.rs`.

use tutti_graph::contract::{drive_input, prepared};
use tutti_graph::{Env, Node, PlayClock, Transport, TransportChanges};
use tutti_nodes::{
    CrossoverNode, EqBandNode, LadderFilterNode, LadderType, SvfControls, SvfFilterNode, SvfType,
};
use tutti_types::{ChannelLayout, Db, Frame, Hz, Q, Resonance, SampleRate, Samples};

const BLOCK: usize = 256;
const BLOCKS: usize = 200;
const RATE: f64 = 48_000.0;

/// Renders a sine of `amp` at `freq` through `node` and returns, per output
/// channel, the RMS of the settled second half over the input's RMS, as the
/// gain in decibels.
fn gains_db<N: Node + 'static>(node: N, freq: f64, amp: f32) -> Vec<f64> {
    let mut node = prepared(node, SampleRate(RATE), BLOCK);
    let width = usize::from(node.shape().audio_in().count());
    let total = BLOCK * BLOCKS;
    let input: Vec<f32> = (0..total)
        .map(|i| amp * (std::f64::consts::TAU * freq * i as f64 / RATE).sin() as f32)
        .collect();
    let mut out: Vec<Vec<f32>> = Vec::new();
    for b in 0..BLOCKS {
        let block = &input[b * BLOCK..(b + 1) * BLOCK];
        let ins: Vec<&[f32]> = (0..width).map(|_| block).collect();
        let env = Env {
            frame: Frame((b * BLOCK) as u64),
            sample_rate: SampleRate(RATE),
            block_len: Samples(BLOCK),
            transport: Transport::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let o = drive_input(&mut node as &mut dyn Node, &env, &ins, &[], &[]).audio;
        if out.is_empty() {
            out = vec![Vec::new(); o.len()];
        }
        for (acc, ch) in out.iter_mut().zip(&o) {
            acc.extend_from_slice(ch);
        }
    }
    let rms = |v: &[f32]| -> f64 {
        let s = &v[v.len() / 2..];
        (s.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
    };
    let reference = rms(&input);
    out.iter()
        .map(|ch| 20.0 * (rms(ch) / reference).log10())
        .collect()
}

fn near(what: &str, got: f64, want: f64, tolerance: f64) {
    assert!(
        (got - want).abs() <= tolerance,
        "{what}: {got:.3} dB, expected {want:.3} +- {tolerance}"
    );
}

/// A bell's gain at its centre is its `gain` (the RBJ peaking filter's
/// definition), and it is unity far from it.
#[test]
fn a_bell_has_its_gain_at_the_centre_and_unity_far_away() {
    let bell = |gain: f32| {
        SvfFilterNode::<f32>::with_controls(
            ChannelLayout::STEREO,
            SvfControls {
                ty: SvfType::Bell,
                freq: Hz(1_500.0),
                q: Q(2.0),
                gain: Db(gain),
            },
        )
    };
    near(
        "+6 dB at the centre",
        gains_db(bell(6.0), 1_500.0, 0.1)[0],
        6.0,
        0.1,
    );
    near(
        "-9 dB at the centre",
        gains_db(bell(-9.0), 1_500.0, 0.1)[0],
        -9.0,
        0.1,
    );
    near("far below", gains_db(bell(6.0), 40.0, 0.1)[0], 0.0, 0.1);
}

/// A high shelf is unity below its corner and its `gain` well above it.
#[test]
fn a_high_shelf_is_unity_below_and_its_gain_above() {
    let shelf =
        |gain: f32| EqBandNode::<f64>::new(SvfType::HighShelf, Hz(4_000.0), Q(0.8), Db(gain));
    near("below", gains_db(shelf(8.0), 100.0, 0.1)[0], 0.0, 0.1);
    near("above", gains_db(shelf(8.0), 18_000.0, 0.1)[0], 8.0, 0.6);
    near(
        "a cut above",
        gains_db(shelf(-5.0), 18_000.0, 0.1)[0],
        -5.0,
        0.6,
    );
}

/// With no resonance a 24 dB ladder low-pass passes DC, is down 1/4 (12.04 dB) at
/// its cutoff (four identical one-pole stages, each 1/sqrt 2 there), and falls
/// 24 dB an octave in the stop band.
#[test]
fn a_ladder_low_pass_has_unity_dc_a_quarter_at_cutoff_and_falls_24_db_an_octave() {
    let ladder = || {
        LadderFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            LadderType::LP24,
            Hz(500.0),
            Resonance::NONE,
        )
    };
    near(
        "a decade below",
        gains_db(ladder(), 20.0, 0.01)[0],
        0.0,
        0.2,
    );
    near(
        "at the cutoff",
        gains_db(ladder(), 500.0, 0.01)[0],
        -12.04,
        0.3,
    );
    let octave = gains_db(ladder(), 2_000.0, 0.01)[0] - gains_db(ladder(), 4_000.0, 0.01)[0];
    near("slope, an octave pair up", octave, 24.0, 1.5);
}

/// A Linkwitz-Riley crossover is -6.02 dB in each neighbouring band at the split,
/// unity in its own passband and gone in the others.
#[test]
fn a_crossover_is_6_db_down_at_each_neighbour_at_the_split() {
    let node = || CrossoverNode::new(ChannelLayout::MONO, &[Hz(300.0), Hz(3_000.0)]).unwrap();
    let at_split = gains_db(node(), 300.0, 0.1);
    assert_eq!(at_split.len(), 3, "three bands of one channel");
    near("low band at the split", at_split[0], -6.02, 0.2);
    near("mid band at the split", at_split[1], -6.02, 0.2);
    let low = gains_db(node(), 30.0, 0.1);
    near("low band, a decade below", low[0], 0.0, 0.1);
    assert!(low[1] < -40.0 && low[2] < -60.0, "{low:?}");
    let high = gains_db(node(), 20_000.0, 0.1);
    near("high band, far above", high[2], 0.0, 0.2);
}
