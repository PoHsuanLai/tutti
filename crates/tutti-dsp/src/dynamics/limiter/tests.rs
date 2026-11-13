use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{BusTopology, Db, SampleRate, Seconds};

use super::*;
use crate::contract::{FamilyCase, Hook, RecurrenceCase, built};
use crate::{Realized, System};

const RATE: SampleRate = SampleRate::SR_48K;
/// 5 ms at 48 kHz: 240 frames.
const LOOKAHEAD: Lookahead = Lookahead::of(Seconds(0.005));
const L: usize = 240;
/// The longest block `run` hands the runner.
const CHUNK: usize = 256;

fn design() -> LimiterFamily {
    LimiterFamily::new(BusTopology::STEREO, LOOKAHEAD)
}

fn controls(threshold: f32, ceiling: f32, release: f64) -> LimiterControls {
    LimiterControls {
        threshold: Db(threshold),
        ceiling: Db(ceiling),
        release: Seconds(release),
    }
}

impl FamilyCase for LimiterFamily {
    fn capacity(&self, _: SampleRate) {}

    fn reachable(&self) -> Vec<LimiterControls> {
        let mut all = Vec::new();
        for threshold in [-30.0, -12.0, -6.0, 0.0] {
            for ceiling in [-12.0, -6.0, -0.3, 0.0] {
                for release in [0.001, 0.02, 0.1, 0.5] {
                    all.push(controls(threshold, ceiling, release));
                }
            }
        }
        let mut rng = StdRng::seed_from_u64(24);
        all.extend((0..64).map(|_| {
            controls(
                rng.gen_range(-40.0..6.0),
                rng.gen_range(-20.0..3.0),
                rng.gen_range(0.0..1.0),
            )
        }));
        all
    }

    /// A release under the floor, and each control not finite (a threshold or a ceiling
    /// only fails at NaN: `+-inf` are levels): each is a member the family can run.
    fn hostile(&self) -> Vec<LimiterControls> {
        let base = controls(-6.0, -0.3, 0.1);
        let mut all = vec![
            LimiterControls {
                release: Seconds(-1.0),
                ..base
            },
            LimiterControls {
                release: Seconds(0.0),
                ..base
            },
        ];
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            all.push(LimiterControls {
                threshold: Db(bad),
                ..base
            });
            all.push(LimiterControls {
                ceiling: Db(bad),
                ..base
            });
            all.push(LimiterControls {
                release: Seconds(f64::from(bad)),
                ..base
            });
        }
        all
    }

    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        &[(
            Hook::Extremes,
            "the envelope is a one-pole inside the unit circle for every release and the \
             window is a finite count: there is no loop to close on its limit",
        )]
    }
}

impl RecurrenceCase for LimiterFamily {
    fn probes(&self) -> Vec<LimiterControls> {
        vec![
            controls(-6.0, -0.3, 0.1),
            controls(-12.0, -12.0, 0.02),
            controls(-20.0, -3.0, 0.3),
        ]
    }

    fn extremes(&self) -> Vec<LimiterControls> {
        Vec::new()
    }

    /// A release that moves, a ceiling that moves a little and a threshold that moves a
    /// little. They land whole, and a limiter's attack is instant, so a step in the
    /// ceiling or the threshold is a step in the gain: it clicks by what it moves, and a
    /// step a few tenths of a dB apart is what a click stays within the contract's bound
    /// for.
    fn steps(&self) -> Vec<(LimiterControls, LimiterControls)> {
        vec![
            (controls(-12.0, -6.0, 0.02), controls(-12.0, -6.0, 0.4)),
            (controls(-12.0, -6.0, 0.1), controls(-12.0, -6.3, 0.1)),
            (controls(-12.0, -6.0, 0.1), controls(-12.4, -6.0, 0.1)),
        ]
    }
}

crate::family_contract!(design());
crate::recurrence_contract!(design(), BlockIndependent);

/// A prepared runner on `family` at `c`.
fn runner_on(family: &LimiterFamily, c: &LimiterControls) -> Realized<LimiterRecurrence> {
    built(family, c, RATE).0
}

fn runner(c: &LimiterControls) -> Realized<LimiterRecurrence> {
    runner_on(&design(), c)
}

/// `audio` (one slice per channel) through `k` in blocks of `block`, with the threshold
/// and ceiling lanes when given.
fn run_in(
    k: &mut Realized<LimiterRecurrence>,
    audio: &[&[f32]],
    lanes: [Option<&[f32]>; 2],
    block: usize,
) -> Vec<Vec<f32>> {
    let n = audio[0].len();
    let mut out = vec![vec![0.0f32; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let a: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
        let mut feed = Feed::new(&a);
        for (i, lane) in lanes.iter().enumerate() {
            if let Some(lane) = lane {
                feed = feed.with_lane(i, &lane[at..end]);
            }
        }
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        k.process(&feed, &mut outs, end - at);
        at = end;
    }
    out
}

fn run(k: &mut Realized<LimiterRecurrence>, audio: &[&[f32]]) -> Vec<Vec<f32>> {
    run_in(k, audio, [None, None], CHUNK)
}

/// The same signal on both channels, run with no lane.
fn run_mono(k: &mut Realized<LimiterRecurrence>, audio: &[f32]) -> Vec<f32> {
    run(k, &[audio, audio]).remove(0)
}

/// `frames` of seeded noise in `-level..=level`.
fn noise(seed: u64, frames: usize, level: f32) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(seed);
    (0..frames).map(|_| rng.gen_range(-level..=level)).collect()
}

/// A level of `db` as an amplitude, from the definition.
fn amplitude(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// The member holds the figures its definition gives: the lookahead is `ceil(lookahead *
/// rate)` frames and is the latency, the tail is none, the release is floored at 1 ms
/// and a non-finite control is the default's (a ceiling only for a NaN).
#[test]
fn the_member_holds_the_figures_of_its_definition() {
    // 5 ms is 220.5 frames at 44.1 kHz, rounded up; exact at the others.
    for (rate, frames) in [(44_100.0, 221), (48_000.0, 240), (96_000.0, 480)] {
        let m = design().member(&LimiterControls::DEFAULT, SampleRate::of(rate));
        assert_eq!(m.latency().get(), frames, "at {rate}");
        assert_eq!(m.tail(Db(-60.0)), Ring::Finite(Seconds(0.0)));
    }
    // A lookahead under one frame is one frame: the window rounds up.
    let m = LimiterFamily::new(BusTopology::MONO, Lookahead::of(Seconds(1e-9)))
        .member(&LimiterControls::DEFAULT, RATE);
    assert_eq!(m.latency().get(), 1);

    let m = design().member(&controls(-12.0, -3.0, 0.0002), RATE);
    assert_eq!(
        m.release(),
        Seconds(0.001),
        "a release under 1 ms reads as 1 ms"
    );
    assert_eq!((m.threshold(), m.ceiling()), (Db(-12.0), Db(-3.0)));

    let d = LimiterControls::DEFAULT;
    let m = design().member(
        &LimiterControls {
            threshold: Db(f32::NAN),
            ceiling: Db(f32::NAN),
            release: Seconds(f64::INFINITY),
        },
        RATE,
    );
    assert_eq!(
        (m.threshold(), m.ceiling(), m.release()),
        (d.threshold, d.ceiling, d.release)
    );
    for inf in [f32::INFINITY, f32::NEG_INFINITY] {
        let m = design().member(
            &LimiterControls {
                ceiling: Db(inf),
                ..d
            },
            RATE,
        );
        assert_eq!(m.ceiling(), Db(inf), "`+-inf` are ceilings");
        let m = design().member(
            &LimiterControls {
                threshold: Db(inf),
                ..d
            },
            RATE,
        );
        assert_eq!(m.threshold(), Db(inf), "`+-inf` are thresholds");
    }
}

/// The recurrence adds no latency of its own: the lookahead is the member's, and a node
/// that sums the two is told it once.
#[test]
fn the_recurrence_adds_no_latency_of_its_own() {
    assert_eq!(runner(&LimiterControls::DEFAULT).latency().get(), 0);
}

/// An independent figure for every output sample, from the definition and not from the
/// code's own helpers: `f64` throughout, the curve in its piecewise form, the envelope
/// by its two rules and the window minimum by brute force over `L + 1` frames. `x` is
/// the input of every channel.
fn oracle(x: &[Vec<f32>], c: &LimiterControls, rate: f64) -> Vec<Vec<f64>> {
    let n = x[0].len();
    let (drive, threshold) = (
        amplitude(f64::from(c.ceiling.get()) - f64::from(c.threshold.get())),
        amplitude(f64::from(c.threshold.get())),
    );
    let coeff = 1.0 - (-1.0 / (c.release.get() * rate)).exp();
    let mut envelope = 0.0f64;
    let mut gain = Vec::with_capacity(n);
    for i in 0..n {
        let peak = x
            .iter()
            .map(|ch| f64::from(ch[i]).abs())
            .fold(0.0, f64::max);
        let target = if peak > threshold {
            drive * threshold / peak
        } else {
            drive
        };
        envelope = if target < envelope {
            target
        } else {
            envelope + coeff * (target - envelope)
        };
        gain.push(envelope);
    }
    // The delayed sample `i` leaves at `i + L`, scaled by the least gain of the frames
    // `i..=i + L`.
    let mut out = vec![vec![0.0f64; n]; x.len()];
    for i in 0..n.saturating_sub(L) {
        let least = gain[i..=i + L]
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min);
        for (o, ch) in out.iter_mut().zip(x) {
            o[i + L] = f64::from(ch[i]) * least;
        }
    }
    out
}

/// Design diagram: every output sample is the input delayed by the lookahead and scaled
/// by the least envelope gain over the window of the sample and the lookahead after it,
/// where the envelope takes the curve's target at once when it is lower and releases by a
/// one-pole otherwise, and the curve is `min(drive, ceiling / peak)`. The figure is an
/// independent `f64` model of the definition, with a brute-force window minimum, over
/// loud and quiet passages and several threshold, ceiling and release settings.
#[test]
fn the_output_is_the_delayed_input_scaled_by_the_window_minimum_of_the_envelope() {
    let loud = |seed: u64, level: f32| -> Vec<f32> {
        let mut x = noise(seed, 12_000, level);
        // Quiet passages between the loud ones.
        for v in &mut x[3000..5000] {
            *v *= 0.02;
        }
        for v in &mut x[8000..9000] {
            *v *= 0.1;
        }
        x
    };
    let x = vec![loud(1, 1.5), loud(2, 0.9)];
    for c in [
        controls(-6.0, -0.3, 0.1),
        controls(-12.0, -12.0, 0.02),
        controls(-20.0, -3.0, 0.005),
        controls(-3.0, -9.0, 0.3),
    ] {
        let mut k = runner(&c);
        let got = run(&mut k, &[&x[0], &x[1]]);
        let want = oracle(&x, &c, RATE.get());
        let mut worst = 0.0f64;
        for (g, w) in got.iter().zip(&want) {
            for (i, (&g, &w)) in g.iter().zip(w).enumerate() {
                let err = (f64::from(g) - w).abs() / w.abs().max(1e-3);
                worst = worst.max(err);
                assert!(err < 2e-4, "{c:?} frame {i}: {g} vs {w}");
            }
        }
        assert!(
            worst > 0.0 || c.release.get() > 0.0,
            "the figure was compared"
        );
        // The figure was not vacuous: some sample was reduced.
        let reduced = got[0]
            .iter()
            .zip(want[0].iter())
            .skip(L)
            .zip(x[0].iter())
            .any(|((_, &w), &x)| w.abs() < 0.5 * f64::from(x).abs());
        assert!(reduced, "{c:?} reduced nothing");
    }
}

/// The output never exceeds the ceiling, at every block length, for loud input: nothing
/// is allowed past it (a gain divide and a product can in principle round one ulp
/// past, so the bound has two ulps' room), whatever the ceiling's order against the
/// threshold.
#[test]
fn the_output_never_exceeds_the_ceiling() {
    let x = [noise(5, 20_000, 2.0), noise(6, 20_000, 1.4)];
    for (threshold, ceiling, release) in
        [(-12.0, -6.0, 0.05), (-20.0, -3.0, 0.2), (-3.0, -9.0, 0.01)]
    {
        let bound = amplitude(ceiling) * (1.0 + 2.0 * f64::from(f32::EPSILON));
        for block in [1, 7, 64, 256] {
            let mut k = runner(&controls(threshold as f32, ceiling as f32, release));
            let out = run_in(&mut k, &[&x[0], &x[1]], [None, None], block);
            let peak = out
                .iter()
                .flatten()
                .map(|&s| f64::from(s).abs())
                .fold(0.0, f64::max);
            assert!(
                peak <= bound,
                "block {block}, ceiling {ceiling} dB: peak {peak} over {bound}"
            );
            // The limiter was at work: it reached for the ceiling, and the input was over it.
            assert!(
                peak > 0.8 * amplitude(ceiling),
                "block {block}: peak {peak}"
            );
        }
        assert!(f64::from(2.0f32) > amplitude(ceiling));
    }
}

/// With a threshold equal to the ceiling the drive is 1: a signal under it passes
/// untouched apart from the delay, once the envelope (which starts at 0) has risen to 1,
/// and a louder one is only ever reduced. And in general a signal under the threshold is
/// moved by the drive, the ceiling over the threshold in dB.
#[test]
fn a_signal_under_the_threshold_is_moved_by_the_drive_and_delayed() {
    // 1 s at a 10 ms release is 100 time constants: the envelope has settled.
    let n = 48_000;
    for (threshold, ceiling, level) in [
        (-6.0f32, -6.0f32, 0.1f32),
        (-20.0, -6.0, 0.03),
        (-6.0, -12.0, 0.2),
    ] {
        let x = noise(7, n, level);
        assert!(
            f64::from(level) < amplitude(f64::from(threshold)),
            "the signal is under the threshold"
        );
        let mut k = runner(&controls(threshold, ceiling, 0.01));
        let out = run_mono(&mut k, &x);
        let drive = amplitude(f64::from(ceiling - threshold));
        for i in n / 2..n {
            let want = f64::from(x[i - L]) * drive;
            assert!(
                (f64::from(out[i]) - want).abs() <= 1e-4 * want.abs().max(1e-3),
                "threshold {threshold}, ceiling {ceiling}, frame {i}: {} vs {want}",
                out[i]
            );
        }
    }
}

/// Before the lookahead has passed nothing has come out, and after it every output is the
/// input a lookahead ago scaled by a gain between 0 and 1 (a ceiling at the threshold is a
/// drive of 1, so the limiter only ever reduces).
#[test]
fn the_output_is_the_input_delayed_by_the_lookahead_times_a_gain_at_most_one() {
    let x = [noise(8, 10_000, 1.5), noise(9, 10_000, 1.0)];
    let mut k = runner(&controls(-6.0, -6.0, 0.05));
    let out = run(&mut k, &[&x[0], &x[1]]);
    for (o, x) in out.iter().zip(&x) {
        assert!(
            o[..L].iter().all(|&s| s == 0.0),
            "silent before the lookahead"
        );
        let mut reduced = false;
        for i in L..o.len() {
            let delayed = x[i - L];
            assert!(o[i] * delayed >= 0.0, "frame {i}: the sign is the input's");
            assert!(
                o[i].abs() <= delayed.abs() * (1.0 + 2.0 * f32::EPSILON),
                "frame {i}: |{}| over |{delayed}|",
                o[i]
            );
            reduced |= o[i].abs() < 0.9 * delayed.abs();
        }
        assert!(reduced, "a loud signal is reduced somewhere");
    }
}

/// The gain is linked across channels, from the loudest: a loud peak on either channel
/// reduces the other too, by the same gain, though the other is under the threshold on its
/// own.
#[test]
fn the_gain_is_linked_across_channels() {
    let n = 12_000;
    let loud = 6_000;
    for loud_channel in 0..2 {
        let mut x = [vec![0.05f32; n], vec![0.05f32; n]];
        x[loud_channel][loud] = 1.0;
        // A release of 5 ms: the envelope (which starts at 0) has settled long before the peak.
        let mut k = runner(&controls(-6.0, -6.0, 0.005));
        let out = run(&mut k, &[&x[0], &x[1]]);
        let quiet = 1 - loud_channel;
        // The quiet channel's gain, where it has a signal, is the loud one's.
        let mut gained = false;
        for i in L..n {
            let (a, b) = (
                out[loud_channel][i] / x[loud_channel][i - L],
                out[quiet][i] / x[quiet][i - L],
            );
            assert!(
                (a - b).abs() <= 1e-6 * a.abs().max(1e-3),
                "loud channel {loud_channel}, frame {i}: {a} vs {b}"
            );
            // Around the peak, where the settled gain is 1 unless the peak reduced it.
            gained |= (loud + 1..loud + L).contains(&i) && b < 0.6;
        }
        assert!(
            gained,
            "the quiet channel was reduced by channel {loud_channel}'s peak"
        );
    }
    // Alone, a quiet channel is untouched (a settled drive of 1).
    let right = vec![0.05f32; n];
    let mut alone = runner(&controls(-6.0, -6.0, 0.05));
    let solo = run(&mut alone, &[&right, &right]);
    assert!(solo[0][n - 1] > 0.049, "{}", solo[0][n - 1]);
}

/// The attack is instant and leads the peak by the whole lookahead, and the release is a
/// one-pole of the coefficient `1 - exp(-1 / (release * rate))`: after a single peak of 1
/// over quiet audio the gain is the curve's `ceiling / peak` at once, for the peak's own
/// sample and the `L` samples before it (their windows hold the peak's frame), not for
/// the one before those; then it climbs back by `1 - (1 - g0) * (1 - c)^k`.
#[test]
fn the_attack_is_instant_and_the_release_is_one_pole() {
    let release = 0.05;
    let n = 60_000;
    let peak_at = 30_000;
    let mut x = vec![0.1f32; n];
    x[peak_at] = 1.0;
    // Threshold at the ceiling: the quiet signal is at gain 1 and the peak's target is
    // the ceiling over the peak, -6 dB.
    let mut k = runner(&controls(-6.0, -6.0, release));
    let out = run_mono(&mut k, &x);
    let g0 = amplitude(-6.0);

    // The peak's own sample is at the target at once.
    let got = f64::from(out[peak_at + L]);
    assert!((got / g0 - 1.0).abs() < 1e-5, "{got} vs {g0}");
    // So are the L samples before it, and not the one before those.
    for j in 1..=L {
        let g = f64::from(out[peak_at - j + L]) / 0.1;
        assert!(
            (g / g0 - 1.0).abs() < 1e-5,
            "{j} before the peak: {g} vs {g0}"
        );
    }
    let before = f64::from(out[peak_at - L - 1 + L]) / 0.1;
    assert!(before > 0.99, "the sample {} before: {before}", L + 1);

    // After it the gain rises by the one-pole's figure, which is checked as a ratio to
    // the gap left: `1 - g_k = (1 - g0) * exp(-k / (release * rate))`.
    let tau = release * RATE.get();
    for k in [1usize, 100, 1000, 2400, 5000] {
        let g = f64::from(out[peak_at + k + L]) / 0.1;
        let want = 1.0 - (1.0 - g0) * (-(k as f64) / tau).exp();
        assert!(
            (g - want).abs() < 2e-4,
            "{k} frames after the peak: {g} vs {want}"
        );
    }
}

/// A threshold and a ceiling lane are read at every frame, not once per block: a ceiling
/// that alternates per frame bounds each output sample by its own frame's ceiling (the
/// sample a lookahead ago, which that frame's gain was made for), and a frame the lane
/// leaves non-finite is bounded by the member's ceiling and does not poison the gain.
#[test]
fn the_ceiling_lane_bounds_each_sample_by_its_own_frames_ceiling() {
    let n = 12_000;
    let x = noise(11, n, 1.5);
    // The member's ceiling is 0 dB, which the lane's -20 and -6 dB are under; a NaN frame
    // is the member's.
    let c = controls(-30.0, 0.0, 0.01);
    let lane: Vec<f32> = (0..n)
        .map(|i| match i % 8 {
            0..=2 => -20.0,
            3 => f32::NAN,
            _ => -6.0,
        })
        .collect();
    let mut k = runner(&c);
    let out = run_in(&mut k, &[&x, &x], [None, Some(&lane)], 100);
    let mut reached = false;
    for i in 0..n - L {
        let bound = if lane[i].is_nan() {
            amplitude(0.0)
        } else {
            amplitude(f64::from(lane[i]))
        } * (1.0 + 4.0 * f64::from(f32::EPSILON));
        let got = f64::from(out[0][i + L]);
        assert!(got.is_finite(), "frame {i}: {got}");
        assert!(
            got.abs() <= bound,
            "frame {i} ({} dB): {got} over {bound}",
            lane[i]
        );
        reached |= lane[i] == -20.0 && got.abs() > 0.5 * amplitude(-20.0);
    }
    assert!(
        reached,
        "the -20 dB frames were limited to their own ceiling"
    );
}

/// The threshold lane is the threshold of its frame: a quiet signal is moved by the
/// drive, so a lane at -30 dB and a base at -10 dB give a drive of 24 dB against 4 dB,
/// and a lane that changes inside a block (not at a block's edge) changes the gain there.
/// A frame the lane leaves non-finite is the member's threshold.
#[test]
fn the_threshold_lane_is_read_per_frame_and_a_non_finite_frame_is_the_members() {
    let n = 12_000;
    let x = vec![0.01f32; n];
    // A 1 ms release settles within a few hundred frames: 40 time constants in 2000.
    let c = controls(-10.0, -6.0, 0.001);
    let drive = |threshold: f64| amplitude(-6.0 - threshold);
    // -30 dB until frame 6000, then the member's -10 dB (as a NaN frame), in blocks of
    // 256: 6000 is not a multiple.
    let lane: Vec<f32> = (0..n)
        .map(|i| if i < 6000 { -30.0 } else { f32::NAN })
        .collect();
    let mut k = runner(&c);
    let out = run_in(&mut k, &[&x, &x], [Some(&lane), None], CHUNK);
    for (i, want) in [(L + 5000, drive(-30.0)), (n - 1, drive(-10.0))] {
        let got = f64::from(out[0][i]) / 0.01;
        assert!(
            (got / want - 1.0).abs() < 1e-3,
            "frame {i}: gain {got} vs {want}"
        );
    }
    // Both the lanes at once, over the loud noise: finite and under the lanes' ceiling.
    let loud = noise(12, n, 2.0);
    let ceiling = vec![-12.0f32; n];
    let thr = vec![-20.0f32; n];
    let mut k = runner(&c);
    let out = run_in(&mut k, &[&loud, &loud], [Some(&thr), Some(&ceiling)], 64);
    let bound = amplitude(-12.0) * (1.0 + 4.0 * f64::from(f32::EPSILON));
    assert!(out[0].iter().all(|&s| f64::from(s).abs() <= bound));
}

/// A moved ceiling lands at once: after a retarget to a lower ceiling, no sample fed from
/// then on comes out over the new one (a glided ceiling would let samples past it while it
/// moved), though the samples fed before it, already in the delay, left at the old.
#[test]
fn a_moved_ceiling_lands_at_once() {
    let n = 24_000;
    let at = 12_000;
    let x = noise(13, n, 2.0);
    let (old, new) = (controls(-12.0, -3.0, 0.1), controls(-12.0, -18.0, 0.1));
    let mut k = runner(&old);
    let mut out = run(&mut k, &[&x[..at], &x[..at]]).remove(0);
    k.retarget(&design().member(&new, RATE));
    out.extend(run(&mut k, &[&x[at..], &x[at..]]).remove(0));

    let bound = amplitude(-18.0) * (1.0 + 4.0 * f64::from(f32::EPSILON));
    // What was fed from the retarget on leaves a lookahead later, every frame under it.
    for (i, &s) in out.iter().enumerate().skip(at + L) {
        assert!(f64::from(s).abs() <= bound, "frame {i}: {s} over {bound}");
    }
    // The old ceiling's output was over the new one: the bound is not trivially met.
    let before = out[..at]
        .iter()
        .map(|&s| f64::from(s).abs())
        .fold(0.0, f64::max);
    assert!(before > 2.0 * bound, "{before}");
}

/// A reset clears the envelope and the delay: the realization renders what a fresh one
/// does, and nothing of the audio before it is heard after (the delay does not hold it).
#[test]
fn a_reset_empties_the_delay() {
    let mut k = runner(&controls(-6.0, -6.0, 0.05));
    run_mono(&mut k, &noise(14, 5_000, 1.0));
    k.reset();
    let out = run_mono(&mut k, &vec![0.0; 2 * L]);
    assert!(
        out.iter().all(|&s| s == 0.0),
        "the old audio came out of the delay"
    );
}

/// Every hostile control runs: the render is finite (a `+inf` ceiling is a boost of the
/// largest finite gain, and `-inf` mutes).
#[test]
fn hostile_controls_render_finite() {
    let x = noise(15, 4_000, 1.0);
    for c in design().hostile() {
        let mut k = runner(&c);
        let out = run_mono(&mut k, &x);
        assert!(out.iter().all(|s| s.is_finite()), "{c:?}");
    }
}

// The shape `check_range_contract` checks is serde's; `Lookahead` has none in the library
// (nothing persists it), so the check runs on the same `try_from` shape written here.
impl serde::Serialize for Lookahead {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for Lookahead {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::try_from(<Seconds as serde::Deserialize>::deserialize(d)?)
            .map_err(serde::de::Error::custom)
    }
}

/// `Lookahead` keeps the range-checked shape of CLAUDE.md: `new` refuses what is not a
/// lookahead (not finite, not above zero, past the cap) and never clamps, `of` agrees,
/// and `TryFrom` carries the refused raw. A silently clamped lookahead would declare a
/// latency the limiter does not have.
#[test]
fn a_lookahead_keeps_the_range_checked_shape() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: Lookahead::new,
        get: Lookahead::get,
        of: Some(Lookahead::of),
        valid: &[Seconds(1e-9), Seconds(0.005), Seconds(0.1), Lookahead::CAP],
        invalid: &[
            Seconds(0.0),
            Seconds(-0.005),
            Seconds(1.000_001),
            Seconds(f64::NAN),
            Seconds(f64::INFINITY),
            Seconds(f64::NEG_INFINITY),
        ],
    });
    assert_eq!(
        Lookahead::try_from(Seconds(-1.0)),
        Err(LookaheadError(Seconds(-1.0)))
    );
}
