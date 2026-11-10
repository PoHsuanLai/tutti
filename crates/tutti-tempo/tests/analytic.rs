//! The map against closed forms, an independent integral and long sessions.
//!
//! Every figure here comes from IEEE `+ - * / sqrt` and `libm`'s pure-Rust
//! `log1p`, `expm1` and `exp`, and a test that needs `ln` calls `libm::log`
//! too. None of it reaches the platform's libm, so the figures hold on any
//! target and no golden digest is needed. Mutations are recorded under each
//! test: "Mutation (run)" means the change was made, the test seen to fail, and
//! the change reverted.

mod common;

use common::{Lcg, R, pt};
use tutti_tempo::{Cursor, Shape, TempoError, TempoMap, TempoPoint};
use tutti_types::{Beat, Bpm, Frame, Seconds};

/// 4 beats at 120 are 2.0 s, frame 96 000; a step reads back the tempo it was
/// given, not `bpm / 60 * 60`.
///
/// Mutation (run): a step's `bpm_at_beat` returns `60.0 * r0` instead of `bpm0`; fails at the
/// first sampled tempo (60.8 BPM does not survive `/ 60 * 60`).
#[test]
fn step_is_beats_over_rate() {
    let m = TempoMap::constant(Bpm::of(120.0)).unwrap();
    assert_eq!(m.seconds_at(Beat(4.0)).get(), 2.0);
    assert_eq!(m.frame_at_or_after(Beat(4.0), R), Frame(96_000));
    assert_eq!(m.beat_at_frame(Frame(96_000), R), Beat(4.0));
    assert_eq!(
        m.seconds_at(Beat(-2.0)).get(),
        -1.0,
        "before beat 0 at the first tempo"
    );
    assert_eq!(
        m.frame_at_or_after(Beat(-2.0), R),
        Frame(0),
        "no frame before 0"
    );
    for bpm in [60.8, 61.3, 61.9, 133.3] {
        let m = TempoMap::constant(Bpm::of(bpm)).unwrap();
        assert_eq!(m.tempo_at(Beat(3.0)), Bpm::of(bpm));
        assert_eq!(m.tempo_at_seconds(Seconds(1.0)), Bpm::of(bpm));
    }
}

/// 120 -> 180 over 4 beats, tempo linear in time. `T = 2*4/(2+3) = 1.6 s`
/// exactly = 76 800 frames; `a = (3-2)/1.6`; `beats(0.8 s) = 2*0.8 + a*0.32 = 1.8`.
///
/// Mutation (run): the `RampTime` inverse's `r0 + sqrt(..)` becomes `r0 - sqrt(..)`; fails on the
/// 1.6 s duration.
#[test]
fn time_ramp_matches_closed_form() {
    let m = TempoMap::new([pt(0.0, 120.0, Shape::RampTime), pt(4.0, 180.0, Shape::Step)]).unwrap();
    assert_eq!(m.frame_at_or_after(Beat(4.0), R), Frame(76_800));
    assert!((m.seconds_at(Beat(4.0)).get() - 1.6).abs() < 1e-15);
    assert!((m.beat_at_seconds(Seconds(0.8)).get() - 1.8).abs() < 1e-15);
    assert!((m.tempo_at_seconds(Seconds(0.8)).get() - 150.0).abs() < 1e-12);
    assert!((m.tempo_at(Beat(1.8)).get() - 150.0).abs() < 1e-12);
    // After the ramp: 180 BPM = 3 beats/s.
    assert!((m.seconds_at(Beat(7.0)).get() - (1.6 + 1.0)).abs() < 1e-15);
    // Frame 38 400 is 0.8 s.
    assert!((m.beat_at_frame(Frame(38_400), R).get() - 1.8).abs() < 1e-14);
}

/// 120 -> 180 over 4 beats, tempo linear in beats. `T = ln(1.5)/g`, `g = 0.25`
/// (beats/s per beat), so `T = 4 ln 1.5` s; the beat where the rate is 2.5
/// (beat 2) is `ln(1.25)/0.25` s in.
///
/// Mutation (run): `log1p(x)` becomes `log1p(x * 0.5)` in `ln_ratio`; fails on `4 ln 1.5`.
#[test]
fn beat_ramp_matches_closed_form() {
    let m = TempoMap::new([
        pt(0.0, 120.0, Shape::RampBeats),
        pt(4.0, 180.0, Shape::Step),
    ])
    .unwrap();
    let t4 = 4.0 * libm::log(1.5);
    assert!(
        (m.seconds_at(Beat(4.0)).get() - t4).abs() < 1e-14,
        "{}",
        m.seconds_at(Beat(4.0)).get()
    );
    // 4 ln 1.5 s is 77 849.3 frames: the first frame at or after is 77 850.
    assert_eq!(m.frame_at_or_after(Beat(4.0), R), Frame(77_850));
    assert!((m.seconds_at(Beat(2.0)).get() - 4.0 * libm::log(1.25)).abs() < 1e-14);
    assert!((m.tempo_at(Beat(2.0)).get() - 150.0).abs() < 1e-12);
    assert!((m.tempo_at_seconds(Seconds(4.0 * libm::log(1.25))).get() - 150.0).abs() < 1e-11);
    assert!((m.beat_at_seconds(Seconds(t4)).get() - 4.0).abs() < 1e-13);
}

/// Independent oracle: integrate `dt = db / r(b)` by Simpson using only
/// `tempo_at` (a different formula from `seconds_at`), over a mixed map.
///
/// Mutation (run): a `RampBeats` `bpm_at_beat` uses `0.9 * k`; the closed-form seconds still
/// hold, so only this oracle (which reads `tempo_at`) fails.
#[test]
fn seconds_agree_with_numeric_integration() {
    let m = TempoMap::new([
        pt(0.0, 90.0, Shape::RampTime),
        pt(8.0, 150.0, Shape::RampBeats),
        pt(20.0, 70.0, Shape::Step),
        pt(30.0, 200.0, Shape::RampTime),
        pt(31.0, 50.0, Shape::Step),
    ])
    .unwrap();
    let simpson = |a: f64, b: f64| {
        let n = 200_000;
        let h = (b - a) / n as f64;
        // The interval's end belongs to the next segment: read just inside it.
        let f = |x: f64| {
            60.0 / m
                .tempo_at(Beat(if x == b { b.next_down() } else { x }))
                .get()
        };
        let mut s = f(a) + f(b);
        for i in 1..n {
            s += f(a + i as f64 * h) * if i % 2 == 1 { 4.0 } else { 2.0 };
        }
        s * h / 3.0
    };
    let mut worst: f64 = 0.0;
    for (a, b) in [
        (0.0, 8.0),
        (8.0, 20.0),
        (20.0, 30.0),
        (30.0, 31.0),
        (31.0, 40.0),
    ] {
        let want = simpson(a, b);
        let got = m.seconds_at(Beat(b)).get() - m.seconds_at(Beat(a)).get();
        worst = worst.max((got - want).abs());
    }
    assert!(worst < 1e-10, "worst {worst:e} s");
}

/// A 5.5-hour session of 20 000 mixed points: beat to seconds to beat, and
/// frame to beat to frame, over 200 000 random queries.
///
/// Mutation (run): `frame_at_or_after` uses a bare `ceil` and drops the `FRAME_TOLERANCE` rule
/// (`first_frame_at_or_after`); the frame round trip mismatches.
#[test]
fn long_session_round_trip() {
    let mut rng = Lcg(7);
    let mut pts = vec![];
    for i in 0..20_000 {
        let shape = match (rng.next() * 3.0) as u32 {
            0 => Shape::Step,
            1 => Shape::RampTime,
            _ => Shape::RampBeats,
        };
        pts.push(pt(
            i as f64 * 2.0,
            60.0 + rng.next() * 140.0,
            if i == 19_999 { Shape::Step } else { shape },
        ));
    }
    let m = TempoMap::new(pts).unwrap();
    let total = m.seconds_at(Beat(40_000.0)).get();
    assert!(total > 3600.0 * 4.0, "a session of hours: {total} s");
    let (mut worst_beat, mut worst_sec) = (0.0f64, 0.0f64);
    let mut bad = 0;
    for _ in 0..200_000 {
        let b = rng.next() * 40_000.0;
        let s = m.seconds_at(Beat(b)).get();
        let back = m.beat_at_seconds(Seconds(s)).get();
        worst_beat = worst_beat.max((back - b).abs());
        worst_sec = worst_sec.max((m.seconds_at(Beat(back)).get() - s).abs());
        let f = Frame((rng.next() * total * 48_000.0) as u64);
        if m.frame_at_or_after(m.beat_at_frame(f, R), R) != f {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "frame -> beat -> frame mismatches");
    assert!(
        worst_beat < 1e-9,
        "beat -> seconds -> beat worst {worst_beat:e} beats"
    );
    assert!(
        worst_sec * 48_000.0 < 1e-4,
        "worst {:e} frames",
        worst_sec * 48_000.0
    );
}

/// Anchors over 100 000 segments: cycle 100, 120, 150, 200 BPM over 1-beat
/// steps (0.6, 0.5, 0.4, 0.3 s), so the anchor of segment `4m` is `9m/5` s. A
/// naive running sum is off by about 1e-8 s; the compensated one by under 1e-10.
///
/// Mutation (run): the compensation term (`comp += ..`) becomes `comp += 0.0`; the anchors drift
/// past 1e-10 s.
#[test]
fn anchors_do_not_drift() {
    let n = 100_000usize;
    let tempo = |i: usize| [100.0, 120.0, 150.0, 200.0][i % 4];
    let m = TempoMap::new((0..n).map(|i| pt(i as f64, tempo(i), Shape::Step))).unwrap();
    let want = |k: usize| (9 * (k / 4)) as f64 / 5.0;
    let mut worst: f64 = 0.0;
    for k in (0..n).step_by(4) {
        worst = worst.max((m.seconds_at(Beat(k as f64)).get() - want(k)).abs());
    }
    assert!(worst < 1e-10, "worst {worst:e} s");
    // The test can fail: a naive sum drifts past the bound.
    let (mut naive, mut naive_worst) = (0.0f64, 0.0f64);
    for k in 0..n {
        if k % 4 == 0 {
            naive_worst = naive_worst.max((naive - want(k)).abs());
        }
        naive += 60.0 / tempo(k);
    }
    assert!(
        naive_worst > 1e-10,
        "the naive sum no longer drifts: {naive_worst:e} s"
    );
}

/// An edit builds a new map: beats before it keep their seconds, beats after it
/// move, and the old map is untouched. Adding at an existing beat replaces.
///
/// Mutation (run): `with_point` inserts at an existing beat instead of replacing it
/// (`Ok(i) => pts.insert(i, point)`); the replace step fails with `BadOrder`.
#[test]
fn edit_in_the_past() {
    let m = TempoMap::new([pt(0.0, 120.0, Shape::Step), pt(16.0, 120.0, Shape::Step)]).unwrap();
    let m2 = m.with_point(pt(8.0, 60.0, Shape::Step)).unwrap();
    assert_eq!(
        m.seconds_at(Beat(4.0)).get(),
        m2.seconds_at(Beat(4.0)).get()
    );
    // 8 beats at 120 (4 s) then 8 beats at 60 (8 s) = 12 s at beat 16, vs 8 s.
    assert_eq!(m2.seconds_at(Beat(16.0)).get(), 12.0);
    assert_eq!(m.seconds_at(Beat(16.0)).get(), 8.0);
    // Beat 20 (was 10 s) is now 12 + 4 beats at 120 (2 s) = 14 s.
    assert_eq!(m2.seconds_at(Beat(20.0)).get(), 14.0);
    // The same beat again replaces the point rather than adding one.
    let m3 = m2.with_point(pt(8.0, 240.0, Shape::Step)).unwrap();
    assert_eq!(m3.points().len(), 3);
    assert_eq!(m3.seconds_at(Beat(16.0)).get(), 4.0 + 2.0);
    assert_eq!(m2.points().len(), 3);
    assert_eq!(
        m2.tempo_at(Beat(9.0)),
        Bpm::of(60.0),
        "the earlier map is unchanged"
    );
}

/// Every refusal, with the index it names. A non-finite or non-positive tempo
/// never gets here: `Bpm` refuses it.
///
/// Mutation (run): `Bpm::new` accepts zero (`raw > 0.0` becomes `raw >= 0.0`); the `-0.0` and
/// `0.0` cases fail.
/// Mutation (run): the last segment's `r0` underflow check in `TempoMap::new` becomes `false`;
/// the smallest-tempo case builds a map and fails.
/// Mutation (run): `is_sound`'s `(back - dsec).abs() <= 1e-9 * dsec` becomes `true`; the
/// squared-overflow ramp is accepted.
#[test]
fn refusals() {
    use Shape::{RampBeats, RampTime, Step};
    assert_eq!(TempoMap::new([]).unwrap_err(), TempoError::Empty);
    assert_eq!(
        TempoMap::new([pt(1.0, 120.0, Step)]).unwrap_err(),
        TempoError::FirstNotAtZero
    );
    assert_eq!(
        TempoMap::new([pt(f64::NAN, 120.0, Step)]).unwrap_err(),
        TempoError::FirstNotAtZero
    );
    // A zero, negative or non-finite tempo is not a `Bpm`, so a map is never
    // handed one and checks none.
    for bad in [
        0.0,
        -120.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.0,
    ] {
        assert_eq!(Bpm::new(bad), None, "{bad}");
    }
    // The smallest tempos the type holds: a beat's rate underflows to zero for
    // the smallest, which the map refuses as unrepresentable, and the next
    // one up builds.
    assert_eq!(
        TempoMap::constant(Bpm::of(f64::from_bits(1))).unwrap_err(),
        TempoError::Unrepresentable(0)
    );
    assert!(TempoMap::constant(Bpm::of(f64::MIN_POSITIVE)).is_ok());
    for beat in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            TempoMap::new([pt(0.0, 120.0, Step), pt(beat, 90.0, Step)]).unwrap_err(),
            TempoError::BadOrder(1),
            "{beat}"
        );
    }
    assert_eq!(
        TempoMap::new([
            pt(0.0, 120.0, Step),
            pt(4.0, 90.0, Step),
            pt(4.0, 80.0, Step)
        ])
        .unwrap_err(),
        TempoError::BadOrder(2)
    );
    assert_eq!(
        TempoMap::new([pt(0.0, 120.0, RampTime)]).unwrap_err(),
        TempoError::RampWithoutEnd
    );
    assert_eq!(
        TempoMap::new([pt(0.0, 120.0, RampBeats)]).unwrap_err(),
        TempoError::RampWithoutEnd
    );
    // A tempo whose square overflows, a span whose duration overflows.
    assert_eq!(
        TempoMap::new([pt(0.0, 1e200, RampTime), pt(4.0, 1e200 * 2.0, Step)]).unwrap_err(),
        TempoError::Unrepresentable(0)
    );
    assert_eq!(
        TempoMap::new([pt(0.0, 1e-300, Step), pt(1e300, 120.0, Step)]).unwrap_err(),
        TempoError::Unrepresentable(0)
    );
    // Squares overflow while the ramp's rate of change stays finite: only the
    // check that the closed forms agree end to end sees it.
    assert_eq!(
        TempoMap::new([pt(0.0, 9e155, RampTime), pt(1.0, 9.009e155, Step)]).unwrap_err(),
        TempoError::Unrepresentable(0)
    );
    // The error reads as a sentence and is a `std::error::Error`.
    let e: Box<dyn std::error::Error> = Box::new(TempoError::Unrepresentable(2));
    assert!(e.to_string().contains("point 2"));
}

/// A hint never changes a result: monotone, backward and random queries agree
/// with cold lookups, bit for bit.
///
/// Mutation (run): the hint check drops its upper bound (`x < key(next)` becomes `true`); a
/// stale hint returns a wrong segment on the backward and random queries.
#[test]
fn cursor_agrees_with_cold_lookup() {
    let mut rng = Lcg(11);
    let pts: Vec<_> = (0..300)
        .map(|i| {
            let shape =
                [Shape::Step, Shape::RampTime, Shape::RampBeats][(rng.next() * 3.0) as usize];
            pt(
                i as f64 * 1.5,
                70.0 + rng.next() * 100.0,
                if i == 299 { Shape::Step } else { shape },
            )
        })
        .collect();
    let m = TempoMap::new(pts).unwrap();
    let mut hint = Cursor::default();
    let mut queries: Vec<f64> = (0..2000).map(|i| i as f64 * 0.25).collect(); // monotone
    queries.extend((0..2000).rev().map(|i| i as f64 * 0.25)); // backward
    queries.extend((0..2000).map(|_| rng.next() * 500.0)); // random
    for b in queries {
        let beat = Beat(b);
        assert_eq!(
            m.seconds_at_with(beat, &mut hint).get(),
            m.seconds_at(beat).get(),
            "beat {b}"
        );
        let s = m.seconds_at(beat).get();
        assert_eq!(
            m.beat_at_seconds_with(Seconds(s), &mut hint),
            m.beat_at_seconds(Seconds(s)),
            "seconds {s}"
        );
        assert_eq!(
            m.frame_at_or_after_with(beat, R, &mut hint),
            m.frame_at_or_after(beat, R)
        );
    }
}

/// A point's own beat belongs to that point, and seconds are continuous across
/// it: the end of a ramp is the start of the next segment.
///
/// Mutation (run): the binary search's `key(s) <= x` becomes `key(s) < x`; a point's own beat
/// falls in the previous segment.
#[test]
fn a_point_belongs_to_the_segment_it_starts() {
    let m = TempoMap::new([
        pt(0.0, 120.0, Shape::RampTime),
        pt(4.0, 180.0, Shape::Step),
        pt(8.0, 90.0, Shape::RampBeats),
        pt(12.0, 60.0, Shape::Step),
    ])
    .unwrap();
    assert_eq!(m.tempo_at(Beat(4.0)), Bpm::of(180.0));
    assert_eq!(m.tempo_at(Beat(8.0)), Bpm::of(90.0));
    assert_eq!(m.tempo_at(Beat(12.0)), Bpm::of(60.0));
    assert!(
        (m.tempo_at(Beat(4.0_f64.next_down())).get() - 180.0).abs() < 1e-9,
        "the ramp arrives at 180"
    );
    for b in [4.0_f64, 8.0, 12.0] {
        let below = m.seconds_at(Beat(b.next_down())).get();
        assert!(
            (m.seconds_at(Beat(b)).get() - below).abs() < 1e-12,
            "seconds jump at beat {b}"
        );
        let s = m.seconds_at(Beat(b)).get();
        assert!(
            (m.beat_at_seconds(Seconds(s)).get() - b).abs() < 1e-12,
            "beat {b}"
        );
    }
}

/// Before beat 0 the map runs at the first tempo, even when the first segment
/// is a ramp (a count-in has a time, and `sqrt` of a negative is not asked).
///
/// Mutation (run): `Seg::sec_in` loses its `db < 0.0` early return; the ramp's `sqrt` of a
/// negative gives NaN before beat 0.
#[test]
fn a_first_ramp_extrapolates_at_its_first_tempo() {
    for shape in [Shape::RampTime, Shape::RampBeats] {
        let m = TempoMap::new([pt(0.0, 120.0, shape), pt(4.0, 240.0, Shape::Step)]).unwrap();
        assert_eq!(m.seconds_at(Beat(-1.0)).get(), -0.5);
        assert_eq!(m.beat_at_seconds(Seconds(-0.5)), Beat(-1.0));
        assert_eq!(m.tempo_at(Beat(-100.0)), Bpm::of(120.0));
        assert_eq!(m.tempo_at_seconds(Seconds(-3.0)), Bpm::of(120.0));
    }
}

/// A tempo is given back to the bit at its own point, ramp or step: the given
/// tempo plus a change of 0, not `60 * (bpm / 60)` (which differs by an ulp
/// for about 5% of tempos).
///
/// Mutation (run): a time ramp's `bpm_at_beat` is `60 * sqrt(r0^2 + 2 k db)`; the tempo at a
/// point is off by an ulp for some tempos.
#[test]
fn a_tempo_at_its_point_is_the_given_tempo_to_the_bit() {
    let mut rng = Lcg(3);
    let n = 10_000;
    let pts: Vec<_> = (0..n)
        .map(|i| {
            let shape = [Shape::Step, Shape::RampTime, Shape::RampBeats][i % 3];
            pt(
                i as f64 * 1.5,
                40.0 + rng.next() * 400.0,
                if i == n - 1 { Shape::Step } else { shape },
            )
        })
        .collect();
    let inexact = pts
        .iter()
        .filter(|p| (p.tempo.get() / 60.0) * 60.0 != p.tempo.get())
        .count();
    assert!(
        inexact > 100,
        "the case has tempos that `/ 60 * 60` changes: {inexact}"
    );
    let m = TempoMap::new(pts.clone()).unwrap();
    for p in &pts {
        assert_eq!(
            m.tempo_at(p.beat).get().to_bits(),
            p.tempo.get().to_bits(),
            "tempo_at {:?}",
            p
        );
        let sec = m.seconds_at(p.beat).get();
        assert_eq!(
            m.tempo_at_seconds(Seconds(sec)).get().to_bits(),
            p.tempo.get().to_bits(),
            "tempo_at_seconds {:?}",
            p
        );
    }
}

/// `-0.0` is beat 0: an edit there replaces the first point.
///
/// Mutation (run): `with_point` drops its `+ 0.0` normalization; `-0.0` is inserted before
/// the point at beat 0 and the map is refused with `BadOrder`.
#[test]
fn an_edit_at_negative_zero_replaces_the_first_point() {
    let m = TempoMap::new([pt(0.0, 120.0, Shape::Step), pt(8.0, 90.0, Shape::Step)]).unwrap();
    let m2 = m.with_point(pt(-0.0, 100.0, Shape::Step)).unwrap();
    assert_eq!(m2.points().len(), 2);
    assert_eq!(m2.tempo_at(Beat(0.0)), Bpm::of(100.0));
    assert_eq!(
        m2.points()[0].beat.get().to_bits(),
        0.0f64.to_bits(),
        "stored as +0.0"
    );
}

/// `constant_tempo` is the one point's tempo to the bit, and `None` for any
/// map that changes, whatever its tempos: two points of the same tempo are
/// still a map with a step in it as far as a reader that follows one tempo
/// goes.
///
/// Mutation (run): `constant_tempo` returning the first point's tempo for any
/// map (`self.pts.first().map(|p| p.tempo)`) -> the two-point maps give
/// `Some` -> fails.
#[test]
fn constant_tempo_is_the_single_points_tempo_to_the_bit() {
    let odd = Bpm::of(97.123_456_789_012_35);
    assert_eq!(TempoMap::constant(odd).unwrap().constant_tempo(), Some(odd));
    for shape in [Shape::Step, Shape::RampTime, Shape::RampBeats] {
        let two = TempoMap::new([
            TempoPoint {
                beat: Beat(0.0),
                tempo: odd,
                shape,
            },
            TempoPoint {
                beat: Beat(4.0),
                tempo: odd,
                shape: Shape::Step,
            },
        ])
        .unwrap();
        assert_eq!(two.constant_tempo(), None, "{shape:?}");
    }
}
