//! Public-API coverage of the modulation matrix.
//!
//! Integration tests see only `pub` items. Default features hide the matrix,
//! so this file is compiled under `--features routing` (and `--all-features`,
//! which implies it).
//!
//! Each test names a property and a mutation that was applied in `src/`
//! (then reverted) to confirm the assertion is load-bearing.

#![cfg(feature = "routing")]

use std::sync::Arc;

use tutti_mod::{
    AtomicTarget, BeatLfo, Curve, EdgeShape, ErasedModulator, LayerKey, LayeredCurve, Lfo,
    LfoShape, ModBus, ModEdge, ModMatrix, ModPreFrame, ModRoutingTable, ModTarget, ModTargetId,
    ShapedCurve, SourceRate, Sourced,
};
use tutti_types::{Beat, BeatDuration, Bpm, Depth, Hz, Phase, PhaseIncrement, Samples, Seconds};

const EPS: f32 = 1e-4;

fn assert_close(got: f32, expected: f32, ctx: &str) {
    assert!(
        (got - expected).abs() < EPS,
        "{ctx}: expected {expected}, got {got}"
    );
}

/// Closed-form sine at cycle position `k * freq * dt` (the free-running
/// driver advances phase *before* sampling, so frame `k` is 1-based).
fn sine_lfo_at_frame(k: usize, freq: Hz, dt: Seconds) -> f32 {
    let phase = Phase::wrapped(k as f32 * freq.get() * dt.get());
    phase.to_radians().sin()
}

/// One cycle per beat, so `run(Beat(p), …)` samples at phase `p`.
fn beat_synced(shape: LfoShape) -> Box<dyn ErasedModulator> {
    Box::new(Sourced::new(
        Lfo::new(shape),
        SourceRate::beat_synced(BeatDuration(1.0), PhaseIncrement(0.0)),
    ))
}

/// A constant offset curve — the public `Curve` stand-in for a frozen layer.
struct ConstCurve(f32);

impl Curve for ConstCurve {
    fn value_at(&self, _beat: Beat) -> Option<f32> {
        Some(self.0)
    }
}

/// Property: a free-running sine LFO at `f` Hz, sampled at frame `k` with
/// step `dt`, traces `base + d · sin(2π · k · f · dt)` at three known phases
/// (quarter / half / three-quarter cycle).
///
/// Mutation: negate the shaped offset in `ModPreFrame::run` → fails `frame 1: expected 0.5261321, got 0.4738679`.
#[test]
fn free_running_sine_traces_closed_form_at_known_phases() {
    let freq = Hz(1.0);
    let frame_rate = Hz(60.0);
    let dt = Seconds(1.0 / frame_rate.get());
    let depth = Depth(0.25);
    let base = 0.5_f32;
    let min = 0.0_f32;
    let max = 1.0_f32;
    // Span is 1, so the target value is exactly `base + depth · lfo(t)`.
    assert_eq!(max - min, 1.0);

    let mut m = ModMatrix::new();
    let target = m.target(base, min, max);
    m.route(
        Lfo::new(LfoShape::Sine),
        SourceRate::free_running(freq, PhaseIncrement(0.0)),
    )
    .to(&target)
    .depth(depth);
    let mut driver = m.build();

    const N: usize = 60;
    let mut at_quarter = None;
    let mut at_half = None;
    let mut at_three_quarter = None;
    for k in 1..=N {
        driver.run(Beat(0.0), dt);
        let got = target.value();
        let expected = base + depth.get() * sine_lfo_at_frame(k, freq, dt);
        // The whole trace follows the closed form, not just the three probes.
        assert_close(got, expected, &format!("frame {k}"));
        match k {
            15 => at_quarter = Some(got),
            30 => at_half = Some(got),
            45 => at_three_quarter = Some(got),
            _ => {}
        }
    }

    // Frame 15: phase = 15/60 = 0.25 → sine peak +1 → 0.5 + 0.25·1 = 0.75.
    assert_close(at_quarter.expect("frame 15"), 0.75, "quarter-cycle peak");
    // Frame 30: phase = 0.50 → sine 0 → base.
    assert_close(at_half.expect("frame 30"), base, "half-cycle zero");
    // Frame 45: phase = 0.75 → sine −1 → 0.5 + 0.25·(−1) = 0.25.
    assert_close(
        at_three_quarter.expect("frame 45"),
        0.25,
        "three-quarter-cycle trough",
    );
}

/// Property: two sources onto one target sum, and swapping the order the
/// routes are inserted does not change any frame's value (structural
/// determinism — `base + Σ offsets` is a set-sum).
///
/// Mutation: `LayeredCurve::value_at` keeps only the first layer (`.next()` not `.sum()`) → fails `order independence at frame 0: expected 0.8, got 0.5`.
#[test]
fn two_sources_sum_and_insertion_order_does_not_matter() {
    let depth_sine = Depth(0.2);
    let depth_square = Depth(0.3);
    let base = 0.5;
    let min = 0.0;
    let max = 1.0;
    let rate = SourceRate::beat_synced(BeatDuration(1.0), PhaseIncrement(0.0));

    let drive = |sine_first: bool| -> Vec<f32> {
        let mut m = ModMatrix::new();
        let t = m.target(base, min, max);
        if sine_first {
            m.route(Lfo::new(LfoShape::Sine), rate.clone())
                .to(&t)
                .depth(depth_sine);
            m.route(Lfo::new(LfoShape::Square), rate.clone())
                .to(&t)
                .depth(depth_square);
        } else {
            m.route(Lfo::new(LfoShape::Square), rate.clone())
                .to(&t)
                .depth(depth_square);
            m.route(Lfo::new(LfoShape::Sine), rate.clone())
                .to(&t)
                .depth(depth_sine);
        }
        let mut driver = m.build();
        // 8 frames covering a full cycle at 1 cycle/beat.
        (0..8)
            .map(|i| {
                driver.run(Beat(i as f64 / 8.0), Seconds(0.0));
                t.value()
            })
            .collect()
    };

    let sine_then_square = drive(true);
    let square_then_sine = drive(false);
    assert_eq!(
        sine_then_square.len(),
        square_then_sine.len(),
        "both orders drive the same number of frames"
    );
    for (i, (a, b)) in sine_then_square
        .iter()
        .zip(square_then_sine.iter())
        .enumerate()
    {
        assert_close(*a, *b, &format!("order independence at frame {i}"));
    }

    // At beat 0.25 both shapes are at +1, so the sum is distinguishable
    // from either source alone: 0.5 + 0.2 + 0.3 = 1.0 (span is 1).
    // Beat 0.25 is frame 2 of the 8-step sweep (i = 2).
    assert_close(
        sine_then_square[2],
        1.0,
        "two sources sum at the shared peak",
    );
    // A last-write-wins collision would have produced 0.7 or 0.8, not 1.0.
}

/// Property: replacing the routing table mid-run with one that drops a
/// route clears that route's layer on the very next frame — no stale
/// offset survives. Restates `hot_swap_clears_the_removed_edges_stale_layer`
/// through the public driver / table API.
///
/// `ModMatrix::build` consumes the table and does not hand it back, so this
/// test wires `ModRoutingTable` + `ModPreFrame` directly (both public).
///
/// Mutation: skip the stale-layer sweep in `ModPreFrame::run` → fails `gain must fall back to base on the frame after its edge is removed: expected 0.5, got 1`.
#[test]
fn hot_swap_clears_the_removed_edges_stale_layer() {
    let cutoff = Arc::new(AtomicTarget::new(1000.0, 0.0, 2000.0));
    let gain = Arc::new(AtomicTarget::new(0.5, 0.0, 1.0));
    let (id_cut, id_gain) = (ModTargetId::next(), ModTargetId::next());
    let bus = Arc::new(ModBus::new());
    bus.insert(id_cut, cutoff.clone());
    bus.insert(id_gain, gain.clone());

    let mut table = ModRoutingTable::new();
    table.set_edges(
        [
            ModEdge::linear(0, id_cut, LayerKey(1), Depth::FULL, 0.0, 2000.0),
            ModEdge::linear(1, id_gain, LayerKey(1), Depth(0.5), 0.0, 1.0),
        ],
        2,
    );
    table.commit();

    let mut driver = ModPreFrame::new(table.snapshot_arc());
    driver.set_router(bus);
    driver.set_sources(vec![
        beat_synced(LfoShape::Sine),
        beat_synced(LfoShape::Triangle),
    ]);

    // Phase 0.25: both sine and triangle sit at +1, so gain is moved off base.
    driver.run(Beat(0.25), Seconds(0.0));
    assert!(
        (gain.final_value() - 0.5).abs() > EPS,
        "gain should be modulated before the swap, got {}",
        gain.final_value()
    );
    let cutoff_while_modulated = cutoff.final_value();
    assert!(
        (cutoff_while_modulated - 1000.0).abs() > EPS,
        "cutoff should be modulated before the swap"
    );

    // Hot-swap: drop the gain edge, keep cutoff.
    table.set_edges(
        [ModEdge::linear(
            0,
            id_cut,
            LayerKey(1),
            Depth::FULL,
            0.0,
            2000.0,
        )],
        2,
    );
    table.commit();
    driver.run(Beat(0.5), Seconds(0.0));

    assert_close(
        gain.final_value(),
        0.5,
        "gain must fall back to base on the frame after its edge is removed",
    );

    // Surviving cutoff edge is still live: a subsequent peak frame must
    // move it off base. (Sine at 0.5 is a zero-crossing, so the swap
    // frame itself is a poor witness.)
    driver.run(Beat(0.25), Seconds(0.0));
    assert!(
        (cutoff.final_value() - 1000.0).abs() > EPS,
        "surviving cutoff edge must still modulate after the swap, got {}",
        cutoff.final_value()
    );
    assert_close(
        gain.final_value(),
        0.5,
        "gain must stay at base on later frames — the stale layer does not return",
    );
}

/// Property: a route whose depth would push past the target's range clamps
/// at min/max, and an authored base outside the range is clamped too.
///
/// Mutation: drop both clamps in `LayeredCurve` → fails `overshoot clamps at max: expected 1, got 1.5`.
#[test]
fn route_and_base_clamp_to_target_range() {
    let rate = SourceRate::beat_synced(BeatDuration(1.0), PhaseIncrement(0.0));

    // Square at +1, full depth, span 1, base 0.5 → 1.5, clamped to max 1.0.
    // Square at −1 (phase 0.75) → −0.5, clamped to min 0.0.
    let mut m = ModMatrix::new();
    let t = m.target(0.5, 0.0, 1.0);
    m.route(Lfo::new(LfoShape::Square), rate)
        .to(&t)
        .depth(Depth::FULL);
    let mut driver = m.build();

    driver.run(Beat(0.0), Seconds(0.0)); // square +1
    assert_close(t.value(), 1.0, "overshoot clamps at max");
    driver.run(Beat(0.75), Seconds(0.0)); // square −1
    assert_close(t.value(), 0.0, "undershoot clamps at min");

    // Out-of-range bases are clamped at construction, before any frame.
    let mut m = ModMatrix::new();
    let high = m.target(1.5, 0.0, 1.0);
    let low = m.target(-0.3, 0.0, 1.0);
    assert_close(high.value(), 1.0, "over-range base clamps at max");
    assert_close(high.target().base(), 1.0, "base() itself is clamped high");
    assert_close(low.value(), 0.0, "under-range base clamps at min");
    assert_close(low.target().base(), 0.0, "base() itself is clamped low");

    // `set_base` is the same clamp, reached through the public `ModTarget`.
    high.target().set_base(9.0);
    assert_close(high.target().base(), 1.0, "set_base clamps high");
    high.target().set_base(-4.0);
    assert_close(high.target().base(), 0.0, "set_base clamps low");
}

/// Property: a beat-synced source locked at one cycle per beat has period
/// equal to the frames-per-beat implied by `(tempo, frame rate)`, and a
/// seek to beat `B` lands on the same phase as running to `B`.
///
/// Mutation: `SourceClock::Synced` accumulates `dt` like `Free` → fails `beat 0 is sine zero: expected 0.5, got 0.5261321`.
#[test]
fn beat_synced_period_matches_tempo_and_seek_is_deterministic() {
    let bpm = Bpm(60.0);
    let frame_rate = Hz(60.0);
    let dt = Seconds(1.0 / frame_rate.get());
    // 60 BPM → 1 beat/s; 60 fps → 60 frames/beat.
    let frames_per_beat =
        Samples((f64::from(frame_rate.get()) * 60.0 / bpm.get()).round() as usize);
    assert_eq!(
        frames_per_beat,
        Samples(60),
        "60 BPM at 60 fps is 60 frames per beat"
    );
    let beats_per_frame = BeatDuration(bpm.get() / 60.0 / f64::from(frame_rate.get()));

    let depth = Depth(0.25);
    let base = 0.5;
    let min = 0.0;
    let max = 1.0;

    let build = || {
        let mut m = ModMatrix::new();
        let t = m.target(base, min, max);
        m.route(
            Lfo::new(LfoShape::Sine),
            SourceRate::beat_synced(BeatDuration(1.0), PhaseIncrement(0.0)),
        )
        .to(&t)
        .depth(depth);
        (m.build(), t)
    };

    let (mut driver, target) = build();
    let mut beat = Beat(0.0);
    let mut at_quarter = None;
    let mut at_origin = None;
    let mut at_next_origin = None;
    // Drive two full beats so the period can be compared against itself.
    let total = frames_per_beat.get() * 2;
    for k in 0..total {
        driver.run(beat, dt);
        let v = target.value();
        if k == 0 {
            at_origin = Some(v);
        }
        if k == frames_per_beat.get() / 4 {
            at_quarter = Some(v);
        }
        if k == frames_per_beat.get() {
            at_next_origin = Some(v);
        }
        beat += beats_per_frame;
    }

    // Beat-synced phase == beat fraction at 1 cycle/beat, *without* the
    // free-running "advance first" offset: frame 0 is phase 0.
    assert_close(at_origin.expect("frame 0"), base, "beat 0 is sine zero");
    assert_close(
        at_quarter.expect("quarter-beat frame"),
        base + depth.get(),
        "beat 0.25 is the sine peak — one quarter of frames_per_beat",
    );
    assert_close(
        at_next_origin.expect("one-beat frame"),
        at_origin.unwrap(),
        "one beat later the phase has wrapped — the period is frames_per_beat",
    );

    // Seek determinism: a fresh driver jumped to beat B equals the sequential
    // run that arrived there. Beat 0.25 is the peak, unambiguous.
    let seek_beat = Beat(0.25);
    let (mut seek_driver, seek_target) = build();
    seek_driver.run(seek_beat, dt);
    assert_close(
        seek_target.value(),
        at_quarter.unwrap(),
        "seek to beat 0.25 matches running to it",
    );

    // Same driver, far seek: beat 100.25 is an integer number of cycles later.
    seek_driver.run(Beat(100.25), Seconds(0.0));
    assert_close(
        seek_target.value(),
        at_quarter.unwrap(),
        "seek far ahead by whole cycles lands on the same phase",
    );

    // Curve path (`BeatLfo`) agrees with the scalar driver at the same beat —
    // the public form of curve_source's seek / scalar-equivalence pin.
    let curve = ShapedCurve::new(
        BeatLfo::new(LfoShape::Sine, BeatDuration(1.0)),
        EdgeShape::new(min, max).with_depth(depth),
    );
    let from_curve = curve
        .value_at(seek_beat)
        .expect("a shaped sine always contributes");
    assert_close(
        base + from_curve,
        seek_target.value(),
        "BeatLfo curve offset plus base matches the driver at the seek beat",
    );
}

/// Property: a constant curve layer and a scalar base produce identical
/// target values at every beat over 1000 frames (scalar / curve
/// equivalence for a frozen contribution).
///
/// Mutation: `Layer::offset_at` adds `1.0` to every curve sample → fails `constant curve vs scalar base at frame 0: expected 0.4, got 1`.
#[test]
fn constant_curve_matches_scalar_base_over_1000_frames() {
    let base = 0.4;
    let min = 0.0;
    let max = 1.0;
    const N: usize = 1000;

    // Scalar path: an unlayered target whose value *is* the authored base,
    // driven 1000 frames with no routes so nothing moves it.
    let mut m = ModMatrix::new();
    let scalar_target = m.target(base, min, max);
    let mut driver = m.build();

    // Curve path: zero base + a constant offset curve of `base`. The curve
    // is an offset (see `ShapedCurve`); adding it to 0 recovers `base`.
    let mut curved: LayeredCurve<f32> = LayeredCurve::new(0.0, min, max);
    curved.set_layer(LayerKey(1), Arc::new(ConstCurve(base)));

    // Same accumulator, scalar layer instead of a curve — the other public
    // delivery of a frozen contribution.
    let mut scalar_layer: LayeredCurve<f32> = LayeredCurve::new(0.0, min, max);
    scalar_layer.set_scalar_layer(LayerKey(1), base);

    for i in 0..N {
        let beat = Beat(i as f64 / 60.0);
        driver.run(beat, Seconds(1.0 / 60.0));
        let from_base = scalar_target.value();
        let from_curve = curved
            .value_at(beat)
            .expect("a constant curve always contributes");
        let from_scalar_layer = scalar_layer
            .value_at(beat)
            .expect("a scalar layer always contributes");
        assert_close(
            from_curve,
            from_base,
            &format!("constant curve vs scalar base at frame {i}"),
        );
        assert_close(
            from_scalar_layer,
            from_base,
            &format!("scalar layer vs scalar base at frame {i}"),
        );
        assert_close(
            from_base,
            base,
            &format!("unlayered base is still {base} at frame {i}"),
        );
    }
}
