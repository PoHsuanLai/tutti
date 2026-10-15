//! `ControlProgram`'s evaluation, corners and refusals, against analytic
//! figures. Each test carries the mutations it was seen to fail under.

use super::*;
use audio_automation::{AutomationEnvelope, AutomationPoint};

const SR: f64 = 48_000.0;

fn at_beat(beat: f64) -> ProgramTime {
    ProgramTime {
        beat: Beat(beat),
        play: SamplePosition(0.0),
        play_epoch: 1,
        rate: SampleRate(SR),
    }
}

fn at_play(play: f64, epoch: u32) -> ProgramTime {
    ProgramTime {
        beat: Beat(0.0),
        play: SamplePosition(play),
        play_epoch: epoch,
        rate: SampleRate(SR),
    }
}

fn amount(depth: f32) -> Amount {
    Amount {
        depth: Depth(depth),
        polarity: Polarity::Bipolar,
        curve: CurveType::Linear,
    }
}

fn layer(id: u64, clock: Clock, shape: LfoShape, amount: Amount) -> Layer {
    Layer {
        id: LayerId(id),
        clock,
        wave: Wave::Shape { shape, seed: 0 },
        amount,
        depth_by: None,
    }
}

fn beats(per_cycle: f64) -> Clock {
    Clock::Beats {
        per_cycle: BeatDuration(per_cycle),
        phase: PhaseIncrement(0.0),
    }
}

fn seconds(rate: f32) -> Clock {
    Clock::Seconds {
        rate: Hz(rate),
        phase: PhaseIncrement(0.0),
        anchor: None,
    }
}

fn program(layers: Vec<Layer>) -> ControlProgram {
    ControlProgram::new(None, layers, vec![]).expect("a valid program")
}

/// A 500 at beat 0 → 0 at beat 4 envelope.
fn ramp() -> Arc<dyn Curve> {
    let mut e = AutomationEnvelope::new(());
    e.add_point(AutomationPoint::new(0.0, 500.0));
    e.add_point(AutomationPoint::new(4.0, 0.0));
    Arc::new(e)
}

/// Automation replaces the knob where it has a value, to the bit (no
/// offsets: the sum starts at `-0.0`, so the base passes through); the knob
/// holds where it has none; neither: no value.
///
/// Mutations (run), each fails: the base as `knob + automation` → 1500
/// reads at beat 0; `.or(knob)` dropped → the empty curve gives `None` with
/// a knob.
#[test]
fn automation_replaces_the_knob() {
    let p = ControlProgram::from_automation(ramp());
    assert_eq!(
        p.value_at(Some(1000.0), &at_beat(0.0), 100.0, &[]),
        Some(500.0)
    );
    assert_eq!(
        p.value_at(Some(1000.0), &at_beat(2.0), 100.0, &[]),
        Some(250.0)
    );
    assert_eq!(p.value_at(None, &at_beat(2.0), 100.0, &[]), Some(250.0));
    let empty: Arc<dyn Curve> = Arc::new(AutomationEnvelope::<()>::new(()));
    let p = ControlProgram::from_automation(empty);
    assert_eq!(
        p.value_at(Some(1000.0), &at_beat(2.0), 100.0, &[]),
        Some(1000.0)
    );
    assert_eq!(p.value_at(None, &at_beat(2.0), 100.0, &[]), None);
    assert!(p.is_pure_automation());
    assert!(!ControlProgram::default().is_pure_automation());
}

/// A layer's offset is `shape(raw) × span`: a ±1 square at depth 0.25 on a
/// span of 800 is ±200, on its edges' sides; a span that is not finite
/// leaves the base alone.
///
/// Mutations (run), each fails: `Amount::offset` not multiplying by the
/// span → ±0.25; the non-finite-span guard dropped → NaN.
#[test]
fn a_layer_offsets_by_depth_times_span() {
    let p = program(vec![layer(1, beats(4.0), LfoShape::Square, amount(0.25))]);
    assert_eq!(
        p.value_at(Some(1000.0), &at_beat(1.0), 800.0, &[]),
        Some(1200.0)
    );
    assert_eq!(
        p.value_at(Some(1000.0), &at_beat(3.0), 800.0, &[]),
        Some(800.0)
    );
    assert_eq!(
        p.value_at(Some(1000.0), &at_beat(3.0), f64::INFINITY, &[]),
        Some(1000.0)
    );
}

/// An input's offset is exactly `depth × span × x` for IEEE-exact figures;
/// an input that has not sent adds nothing, even unipolar (whose raw 0
/// would be half its depth).
///
/// Mutations (run), each fails: every input layer reading the first slot
/// (`inputs.first()` for `inputs.get(k)`) → the second input reads the
/// first's value; a missing input read as raw 0 → the unipolar one adds 0.5 × depth.
#[test]
fn an_inputs_offset_is_depth_times_span_times_x() {
    let inputs = vec![
        InputLayer {
            id: LayerId(1),
            source: SourceId(40),
            amount: amount(0.5),
        },
        InputLayer {
            id: LayerId(2),
            source: SourceId(41),
            amount: Amount {
                depth: Depth(0.25),
                polarity: Polarity::Unipolar,
                curve: CurveType::Linear,
            },
        },
    ];
    let p = ControlProgram::new(None, vec![], inputs).expect("valid");
    // 0.5 × 64 × 0.75 = 24.
    assert_eq!(
        p.value_at(Some(100.0), &at_beat(0.0), 64.0, &[Some(0.75), None]),
        Some(124.0)
    );
    // Unipolar x = 1: 0.25 × 64 × 1 = 16.
    assert_eq!(
        p.value_at(Some(100.0), &at_beat(0.0), 64.0, &[Some(-0.25), Some(1.0)]),
        Some(100.0 - 8.0 + 16.0)
    );
}

/// Layers sum in `f64` and round once: the offsets sum to
/// 11 000 000.0149 and the knob is 0.5, so the `f64` sum rounds up to
/// 11 000 001, where any intermediate `f32` rounding lands on the tie
/// 11 000 000.5 and rounds to even, 11 000 000. Insertion order does not
/// matter: `new` sorts by id.
///
/// Mutations (run), each fails: summing in `f32` per layer
/// (`acc = f64::from(acc as f32 + o as f32)`) → 11 000 000; the offsets
/// summed in `f64` but rounded before the base is added
/// (`base + acc as f32`) → 11 000 000.
#[test]
fn layers_sum_in_id_order_in_f64() {
    let l = |id, d| layer(id, beats(4.0), LfoShape::Square, amount(d));
    let a = program(vec![l(3, 0.3), l(1, 0.1), l(2, 0.7)]);
    let b = program(vec![l(1, 0.1), l(2, 0.7), l(3, 0.3)]);
    assert_eq!(a, b);
    let span = 1.0e7;
    let want =
        (0.5f64 + f64::from(0.1f32) * span + f64::from(0.7f32) * span + f64::from(0.3f32) * span)
            as f32;
    assert_eq!(want, 11_000_001.0);
    assert_eq!(a.value_at(Some(0.5), &at_beat(1.0), span, &[]), Some(want));
}

/// Phase is a function of time, not of the program: a depth edit (a new
/// program) evaluates the same wave at the same phase, scaled. For a sine
/// at depth 0.3 then 0.6, the offset doubles at every sampled time.
///
/// A program has no age to offset phase by, so the property is the closed
/// form itself; the lane test `an_lfos_phase_survives_a_depth_edit` pins it
/// across a republish. Mutation (run): `Clock::cycles` ignoring `phase` for
/// a seconds clock → the 0.125 offset is lost → fails.
#[test]
fn phase_survives_a_depth_edit() {
    let clock = Clock::Seconds {
        rate: Hz(3.0),
        phase: PhaseIncrement(0.125),
        anchor: None,
    };
    let a = program(vec![layer(1, clock, LfoShape::Sine, amount(0.3))]);
    let b = program(vec![layer(1, clock, LfoShape::Sine, amount(0.6))]);
    for k in 0..50 {
        let t = at_play(f64::from(k) * 977.0, 5);
        let va = f64::from(a.value_at(Some(0.0), &t, 10.0, &[]).unwrap());
        let vb = f64::from(b.value_at(Some(0.0), &t, 10.0, &[]).unwrap());
        let phase = 3.0 * f64::from(k) * 977.0 / SR + 0.125;
        let want = (phase * std::f64::consts::TAU).sin() * 3.0;
        assert!((va - want).abs() < 1e-5, "{k}: {va} vs {want}");
        assert!((vb - 2.0 * va).abs() < 1e-5, "{k}: {vb} vs 2 × {va}");
    }
}

/// A rate anchor in the current epoch continues the phase through a rate
/// change: at the anchor the phase is the anchor's, before it the old rate
/// runs and after it the new one; the anchor's frame is a corner. An
/// anchor from another epoch is ignored.
///
/// Mutations (run), each fails: the anchor applied whatever the epoch → the
/// stale-epoch figure moves; the old rate used after the anchor → the slope
/// after it is wrong; no corner at the anchor → `next_corner` reports the
/// wave's corner past it.
#[test]
fn a_rate_anchor_continues_the_phase() {
    let anchor = RateAnchor {
        epoch: 9,
        at: Frame(48_000),
        phase: 10.25,
        prev_rate: Hz(2.0),
    };
    let clock = Clock::Seconds {
        rate: Hz(4.0),
        phase: PhaseIncrement(0.0),
        anchor: Some(anchor),
    };
    assert_eq!(clock.cycles(&at_play(48_000.0, 9)), Some(10.25));
    assert_eq!(clock.cycles(&at_play(24_000.0, 9)), Some(9.25));
    assert_eq!(clock.cycles(&at_play(60_000.0, 9)), Some(11.25));
    assert_eq!(clock.cycles(&at_play(24_000.0, 8)), Some(2.0));
    // At play 40 800 the layer is at cycle 9.95 (rate 2 back from the
    // anchor): a square's next edge, cycle 10.0, is at play 42 000, before
    // the anchor; from there the next (10.5) would be past it, so the
    // anchor itself is the corner.
    let p = program(vec![layer(1, clock, LfoShape::Square, amount(1.0))]);
    assert_eq!(
        p.next_corner(&at_play(40_800.0, 9)).play,
        Some(SamplePosition(42_000.0))
    );
    assert_eq!(
        p.next_corner(&at_play(42_000.0, 9)).play,
        Some(SamplePosition(48_000.0))
    );
    // From the anchor: edge 10.5 at rate 4 is 3000 frames on.
    assert_eq!(
        p.next_corner(&at_play(48_000.0, 9)).play,
        Some(SamplePosition(51_000.0))
    );
}

/// Each wave's corners, on a beat clock of 4 beats a cycle: a square's two
/// edges, a triangle's peaks, a saw's wrap, a random's steps; a sine has
/// none unless a curved bipolar amount kinks it at its zero crossings. The
/// next corner is strictly after the time, even when the time is one.
///
/// Mutations (run), each fails: `next_corner` taking a corner at the time
/// itself (`>=`) → the square's corner from beat 2 is 2; the kink set
/// ignoring the curve (`kinks_at_zero` always false) → the curved sine has
/// no corner; the triangle's peaks at 0 and 0.5 (its zero crossings) → 1.0
/// is not a corner.
#[test]
fn each_wave_has_its_corners() {
    let corner = |shape, amount, beat| {
        program(vec![layer(1, beats(4.0), shape, amount)])
            .next_corner(&at_beat(beat))
            .beat
    };
    let lin = amount(1.0);
    let curved = Amount {
        curve: CurveType::Exponential,
        ..lin
    };
    assert_eq!(corner(LfoShape::Square, lin, 0.5), Some(Beat(2.0)));
    assert_eq!(corner(LfoShape::Square, lin, 2.0), Some(Beat(4.0)));
    assert_eq!(corner(LfoShape::Triangle, lin, 0.0), Some(Beat(1.0)));
    assert_eq!(corner(LfoShape::Triangle, lin, 1.0), Some(Beat(3.0)));
    assert_eq!(corner(LfoShape::Triangle, curved, 1.0), Some(Beat(2.0)));
    assert_eq!(corner(LfoShape::Sawtooth, lin, 1.0), Some(Beat(4.0)));
    assert_eq!(corner(LfoShape::Sawtooth, curved, 1.0), Some(Beat(2.0)));
    assert_eq!(corner(LfoShape::Random, lin, 5.0), Some(Beat(8.0)));
    assert_eq!(corner(LfoShape::Sine, lin, 1.0), None);
    assert_eq!(corner(LfoShape::Sine, curved, 1.0), Some(Beat(2.0)));
    let uni = Amount {
        polarity: Polarity::Unipolar,
        ..curved
    };
    assert_eq!(corner(LfoShape::Sine, uni, 1.0), None, "unipolar: no kink");
}

/// The automation's next breakpoint is a corner, and a seconds layer's
/// corner is on the play clock: `Corners` keeps the two apart.
///
/// Mutation (run): the automation's corner ignored (`next = None`) → no
/// beat corner → fails.
#[test]
fn automation_and_seconds_corners_are_on_their_clocks() {
    let p = ControlProgram::new(
        Some(ramp()),
        vec![layer(1, seconds(2.0), LfoShape::Square, amount(0.1))],
        vec![],
    )
    .expect("valid");
    let c = p.next_corner(&at_play(1_000.0, 1));
    assert_eq!(c.beat, Some(Beat(4.0)));
    // Edge 0.5 at 2 Hz is 12 000 frames.
    assert_eq!(c.play, Some(SamplePosition(12_000.0)));
}

/// The left limit differs from the value only at a jump: on a square's edge
/// it is the level before, a frame later they agree.
///
/// The automation's left limit is the curve's just before the time: on a
/// stepped breakpoint, the step's level.
///
/// Mutations (run), each fails: `value_before` evaluating at the time
/// itself (`left` ignored in `raw_at`) → the edge reads the level after;
/// the automation read at the time itself under `left` → the breakpoint's
/// left limit reads 300; the piece's position not rounded down to `f32`
/// → it rounds up to 1.0 and reads 300.
#[test]
fn the_left_limit_is_the_level_before_a_jump() {
    let p = program(vec![layer(1, beats(4.0), LfoShape::Square, amount(1.0))]);
    assert_eq!(p.value_at(Some(0.0), &at_beat(2.0), 1.0, &[]), Some(-1.0));
    assert_eq!(
        p.value_before(Some(0.0), &at_beat(2.0), 1.0, &[]),
        Some(1.0)
    );
    assert_eq!(
        p.value_before(Some(0.0), &at_beat(2.5), 1.0, &[]),
        Some(-1.0)
    );
    let r = program(vec![layer(1, beats(1.0), LfoShape::Random, amount(1.0))]);
    let before = r.value_before(Some(0.0), &at_beat(3.0), 1.0, &[]);
    assert_eq!(before, r.value_at(Some(0.0), &at_beat(2.5), 1.0, &[]));
    assert_ne!(before, r.value_at(Some(0.0), &at_beat(3.0), 1.0, &[]));
    let mut e = AutomationEnvelope::new(());
    e.add_point(AutomationPoint::with_curve(0.0, 100.0, CurveType::Stepped));
    e.add_point(AutomationPoint::new(2.0, 300.0));
    let a = ControlProgram::from_automation(Arc::new(e));
    assert_eq!(a.value_at(None, &at_beat(2.0), 1.0, &[]), Some(300.0));
    assert_eq!(a.value_before(None, &at_beat(2.0), 1.0, &[]), Some(100.0));
}

/// `depth_by` scales a layer's offset by a second wave mapped to `[0, 1]`,
/// and its corners are the layer's too: a ±1 square on 4 beats scaled by a
/// square on 8 is +1, then −1, then 0 while the depth wave is low; from
/// beat 0.5 the next corner is the depth wave's (a square on 3: beat 1.5),
/// before the layer's own (beat 2).
///
/// Mutations (run), each fails: the scale not applied (`off *= m` dropped)
/// → beat 5 reads +1; the depth wave's corners ignored → the corner is 2.
#[test]
fn depth_by_scales_the_offset_and_adds_its_corners() {
    let scaled = |depth_clock| Layer {
        depth_by: Some((
            depth_clock,
            Wave::Shape {
                shape: LfoShape::Square,
                seed: 0,
            },
        )),
        ..layer(1, beats(4.0), LfoShape::Square, amount(1.0))
    };
    let p = program(vec![scaled(beats(8.0))]);
    assert_eq!(p.value_at(Some(0.0), &at_beat(1.0), 1.0, &[]), Some(1.0));
    assert_eq!(p.value_at(Some(0.0), &at_beat(3.0), 1.0, &[]), Some(-1.0));
    assert_eq!(p.value_at(Some(0.0), &at_beat(5.0), 1.0, &[]), Some(0.0));
    let p = program(vec![scaled(beats(3.0))]);
    assert_eq!(p.next_corner(&at_beat(0.5)).beat, Some(Beat(1.5)));
}

/// A curve that reads one value and freezes to another: what a program
/// holds is the frozen copy.
struct Thawed;
impl Curve for Thawed {
    fn value_at(&self, _: Beat) -> Option<f32> {
        Some(1.0)
    }
    fn frozen(&self) -> Option<Arc<dyn Curve>> {
        let mut e = AutomationEnvelope::new(());
        e.add_point(AutomationPoint::new(0.0, 2.0));
        Some(Arc::new(e))
    }
}

/// Both constructors take the curve's frozen copy, so a lane never reads a
/// live cell through it.
///
/// Mutations (run), each fails: `new` keeping the curve as given → 1.0;
/// `from_automation` keeping it → 1.0.
#[test]
fn a_program_holds_the_frozen_curve() {
    let p = ControlProgram::new(Some(Arc::new(Thawed)), vec![], vec![]).expect("valid");
    assert_eq!(p.value_at(None, &at_beat(1.0), 1.0, &[]), Some(2.0));
    let p = ControlProgram::from_automation(Arc::new(Thawed));
    assert_eq!(p.value_at(None, &at_beat(1.0), 1.0, &[]), Some(2.0));
}

/// Random steps replay: the same cycle gives the same value whatever the
/// program's history, a seed moves the sequence, and the smooth shape
/// meets each step at the cycle's start.
///
/// Mutation (run): `RandomSmooth` interpolating `hash(n)` to `hash(n + 1)`
/// → a cycle's start reads its own step, not the one before → fails.
#[test]
fn random_waves_replay_by_cycle() {
    let w = Wave::Shape {
        shape: LfoShape::Random,
        seed: 0,
    };
    let s = Wave::Shape {
        shape: LfoShape::Random,
        seed: 7,
    };
    let smooth = Wave::Shape {
        shape: LfoShape::RandomSmooth,
        seed: 0,
    };
    for n in 0..20 {
        let c = f64::from(n);
        assert_eq!(w.raw_at(c + 0.3, false), hash_bipolar(n.into()));
        assert_eq!(w.raw_at(c + 0.3, false), w.raw_at(c + 0.9, false));
        // The smooth wave at a cycle's start is the step before it, and
        // runs to the cycle's own step.
        assert_eq!(smooth.raw_at(c, false), hash_bipolar(i64::from(n) - 1));
        assert!((smooth.raw_at(c + 1.0, true) - hash_bipolar(n.into())).abs() < 1e-5);
    }
    assert!((0..20).any(|n| s.raw_at(f64::from(n), false) != w.raw_at(f64::from(n), false)));
}

/// A drawn period: its points' values on their positions, its pieces along
/// their curves, the last piece wrapping to the next period's first point,
/// and a curved bipolar layer's zero crossings as corners.
///
/// Mutations (run), each fails: the last piece running to 1.0 instead of
/// the next period's first point (`1.0 + first.at`) → the value at 0.95
/// reads halfway, not a quarter; the crossing solved as linear for every
/// curve → the exponential piece's crossing is at 2/3 of the piece instead
/// of √(2/3).
#[test]
fn a_drawn_wave_follows_its_points() {
    let points: Arc<[PeriodPoint]> = Arc::from(vec![
        PeriodPoint {
            at: Phase(0.1),
            value: 1.0,
            curve: CurveType::Linear,
        },
        PeriodPoint {
            at: Phase(0.5),
            value: -1.0,
            curve: CurveType::Exponential,
        },
        PeriodPoint {
            at: Phase(0.9),
            value: 0.5,
            curve: CurveType::Linear,
        },
    ]);
    let w = Wave::Drawn(Arc::clone(&points));
    let near = |c: f64, want: f32| {
        let v = w.raw_at(c, false);
        assert!((v - want).abs() < 1e-6, "at {c}: {v} vs {want}");
    };
    near(0.1, 1.0);
    near(0.3, 0.0);
    near(1.5, -1.0);
    // The wrap piece: 0.5 at 0.9 to 1.0 at 1.1.
    near(0.95, 0.625);
    near(0.05, 0.875);
    // The exponential piece from -1 to 0.5: -1 + 1.5 t² crosses at
    // t = √(2/3).
    let want = 0.5 + 0.4 * (2.0f64 / 3.0).sqrt();
    let kinked = w.next_corner(0.6, true, true).expect("a crossing");
    assert!((kinked - want).abs() < 1e-6, "{kinked} vs {want}");
    assert_eq!(w.next_corner(0.6, true, false), Some(0.9f32.into()));
}

/// `new` refuses past the lane's fixed state, duplicate ids and a bad
/// drawing, by name.
///
/// Mutation (run): the drawn check dropped → the unsorted drawing is
/// accepted → fails.
#[test]
fn new_refuses_what_a_lane_cannot_hold() {
    let l = |id| layer(id, beats(1.0), LfoShape::Sine, amount(0.1));
    assert_eq!(
        ControlProgram::new(None, (0..17).map(l).collect(), vec![]).unwrap_err(),
        ProgramError::TooManyLayers { count: 17 }
    );
    let input = |id| InputLayer {
        id: LayerId(id),
        source: SourceId(1),
        amount: amount(0.1),
    };
    assert_eq!(
        ControlProgram::new(None, vec![], (0..9).map(input).collect()).unwrap_err(),
        ProgramError::TooManyInputs { count: 9 }
    );
    assert_eq!(
        ControlProgram::new(None, vec![l(4), l(4)], vec![]).unwrap_err(),
        ProgramError::DuplicateLayer(LayerId(4))
    );
    let bad = Layer {
        wave: Wave::Drawn(Arc::from(vec![
            PeriodPoint {
                at: Phase(0.5),
                value: 0.0,
                curve: CurveType::Linear,
            },
            PeriodPoint {
                at: Phase(0.2),
                value: 0.0,
                curve: CurveType::Linear,
            },
        ])),
        ..l(5)
    };
    assert_eq!(
        ControlProgram::new(None, vec![bad], vec![]).unwrap_err(),
        ProgramError::BadDrawnWave(LayerId(5))
    );
    assert!(ControlProgram::new(None, (0..16).map(l).collect(), vec![]).is_ok());
}
