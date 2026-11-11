//! Construction arguments and gain cells that are not finite: a constructor
//! refuses or saturates (never panics), and a gain cell holding NaN or an
//! infinity never reaches the output.

use std::sync::atomic::Ordering;

use tutti_graph::contract::drive;
use tutti_graph::{Node, ParamNode, Prepare};
use tutti_nodes::{
    BrickwallLimiterNode, BusStripNode, DelayLineNode, EqBandNode, LimiterNode, SvfType,
};
use tutti_types::{
    AtomicF32, Db, Feedback, Hz, ParamCell, Q, SampleRate, Samples, Seconds, UnitParam,
};

const RATE: SampleRate = SampleRate(48_000.0);
const BAD: [f32; 3] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

fn block(width: usize, seed: usize) -> Vec<Vec<f32>> {
    (0..width)
        .map(|c| {
            (0..64)
                .map(|i| ((i + c + seed) as f32 * 0.3).sin() * 0.5)
                .collect()
        })
        .collect()
}

fn renders_finite(node: &mut dyn Node, blocks: usize) -> bool {
    let width = usize::from(node.shape().audio_in().count());
    let mut ok = true;
    for k in 0..blocks {
        let chans = block(width, k);
        let refs: Vec<&[f32]> = chans.iter().map(|c| &c[..]).collect();
        ok &= drive(node, RATE, &refs, &[])
            .iter()
            .flatten()
            .all(|s| s.is_finite());
    }
    ok
}

/// A maximum delay that is NaN, infinite or negative saturates to an empty
/// line (`Seconds::to_samples_ceil` reads a non-finite span as zero frames):
/// the node still builds, passes audio through its zero-length line and stays
/// finite. It panicked in `f32::clamp(0.0, max)` before.
///
/// Mutation (run): `max_delay = max_delay_secs.into()` with no saturation
/// makes the NaN, -inf and negative cases panic.
#[test]
fn a_delay_with_an_unusable_maximum_saturates_to_an_empty_line() {
    for max in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, -1e30] {
        for node in [
            DelayLineNode::new(Seconds(max), Seconds(0.01), Feedback(0.3)),
            DelayLineNode::stereo(Seconds(max), Seconds(0.01), Seconds(0.02), Feedback(0.3)),
        ] {
            let mut node = node;
            node.prepare(&Prepare::new(RATE, Samples(64)));
            assert!(renders_finite(&mut node, 4), "max {max}");
        }
    }
}

/// A NaN initial delay time is read as zero, whatever the maximum.
///
/// Mutation (run): in `with_channels`, `delay_secs.clamp(..)` without the
/// `is_finite` test leaves NaN in the cell and the case fails.
#[test]
fn a_delay_with_a_nan_initial_time_starts_at_zero() {
    let node = DelayLineNode::new(Seconds(0.1), Seconds(f64::NAN), Feedback(0.3));
    assert_eq!(node.delay_time().load(Ordering::Acquire), 0.0);
}

/// The strip's volume and balance cells hold a bad write off: the output stays
/// finite while it is there and after a finite write.
///
/// Mutation: dropping the `LastGood` read of the volume in `control_gains`
/// fails the volume cases (run). The pan cases pin that a NaN or infinite
/// balance is already finite without a guard.
#[test]
fn the_strip_holds_off_non_finite_volume_and_pan() {
    type Cell = fn(&BusStripNode) -> std::sync::Arc<AtomicF32>;
    let cells: [(&str, Cell); 2] = [("volume", BusStripNode::volume), ("pan", BusStripNode::pan)];
    for (name, cell) in cells {
        for bad in BAD {
            let mut node = BusStripNode::new();
            node.prepare(&Prepare::new(RATE, Samples(64)));
            assert!(renders_finite(&mut node, 2));
            let atomic = cell(&node);
            atomic.store(bad, Ordering::Release);
            assert!(
                renders_finite(&mut node, 4),
                "{name} = {bad} reached the output"
            );
            atomic.store(0.5, Ordering::Release);
            assert!(
                renders_finite(&mut node, 4),
                "{name} = {bad} left the strip non-finite"
            );
        }
    }
}

/// The limiters' and the EQ band's control cells already hold a bad write off
/// (their `LastGood` reads): probed on 2026-11 against NaN and both infinities,
/// written beside a moving control and again after a finite value, and the
/// output stayed finite. Kept as a regression guard; it found nothing to fix.
///
/// No mutation of this test is claimed: dropping the limiter release's
/// `LastGood` read is not caught here (the follower guards a NaN behind it, as
/// `non_finite_controls.rs` notes for the compressor's attack), so it pins the
/// behaviour and not each guard.
#[test]
fn the_limiters_and_the_eq_band_hold_off_non_finite_controls() {
    type Cell = (&'static str, ParamCell, f32);
    fn check(name: &str, make: impl Fn() -> (Box<dyn Node>, Vec<Cell>)) {
        for bad in BAD {
            for k in 0..make().1.len() {
                let (mut node, cells) = make();
                node.prepare(&Prepare::new(RATE, Samples(64)));
                assert!(renders_finite(&mut *node, 2), "{name}");
                for (j, (_, cell, alt)) in cells.iter().enumerate() {
                    cell.store(if j == k { bad } else { *alt }, Ordering::Release);
                }
                assert!(
                    renders_finite(&mut *node, 6),
                    "{name}: {} = {bad} reached the output",
                    cells[k].0
                );
                cells[k].1.store(cells[k].2, Ordering::Release);
                assert!(
                    renders_finite(&mut *node, 6),
                    "{name}: {} = {bad} left the state non-finite",
                    cells[k].0
                );
            }
        }
    }
    check("limiter", || {
        let n = LimiterNode::new(Db(-6.0), Db(-1.0));
        let cells = vec![
            ("threshold", n.threshold().into(), -12.0),
            ("ceiling", n.ceiling().into(), -3.0),
            ("release", n.release_time().into(), 0.2),
        ];
        (Box::new(n), cells)
    });
    check("brickwall", || {
        let n = BrickwallLimiterNode::new(Db(-1.0));
        let cells = vec![("ceiling", n.ceiling().into(), -3.0)];
        (Box::new(n), cells)
    });
    check("eq band", || {
        let n = EqBandNode::<f64>::new(SvfType::Bell, Hz(1000.0), Q(1.0), Db(3.0));
        let cells = vec![
            (
                "frequency",
                n.param_set().cell(UnitParam::Cutoff).unwrap(),
                2000.0,
            ),
            ("q", n.param_set().cell(UnitParam::Q).unwrap(), 2.0),
            ("gain", n.param_set().cell(UnitParam::GainDb).unwrap(), 6.0),
        ];
        (Box::new(n), cells)
    });
}
