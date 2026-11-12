use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{BusTopology, Db, SampleRate, Seconds};

use super::*;
use crate::contract::{FamilyCase, Hook, RecurrenceCase, built};
use crate::{Realized, Setup};

const RATE: SampleRate = SampleRate(48_000.0);
/// The longest block `run` hands the runner.
const CHUNK: usize = 256;

fn design() -> GateFamily {
    GateFamily::new(BusTopology::STEREO, Key::Sidechain)
}

fn own() -> GateFamily {
    GateFamily::new(BusTopology::STEREO, Key::Own)
}

fn controls(threshold: f32, attack: f64, hold: f64, release: f64, range: f32) -> GateControls {
    GateControls {
        threshold: Db(threshold),
        attack: Seconds(attack),
        hold: Seconds(hold),
        release: Seconds(release),
        range: Db(range),
    }
}

impl FamilyCase for GateFamily {
    fn capacity(&self, _: SampleRate) {}

    fn reachable(&self) -> Vec<GateControls> {
        let mut all = Vec::new();
        for threshold in [-60.0, -30.0, -12.0, 0.0] {
            for range in [-80.0, -24.0, -6.0, 0.0] {
                for (attack, hold, release) in
                    [(0.0, 0.0, 0.0), (0.001, 0.02, 0.05), (0.01, 0.1, 0.3)]
                {
                    all.push(controls(threshold, attack, hold, release, range));
                }
            }
        }
        let mut rng = StdRng::seed_from_u64(24);
        all.extend((0..64).map(|_| {
            controls(
                rng.gen_range(-70.0..6.0),
                rng.gen_range(0.0..0.05),
                rng.gen_range(0.0..0.3),
                rng.gen_range(0.0..0.5),
                rng.gen_range(-90.0..0.0),
            )
        }));
        all
    }

    /// Below the floors (a time under 0), above the cap (a range over 0 dB) and not
    /// finite: each is a member the family can run.
    fn hostile(&self) -> Vec<GateControls> {
        let base = controls(-30.0, 0.003, 0.05, 0.08, -40.0);
        let mut all = vec![
            GateControls {
                attack: Seconds(-1.0),
                hold: Seconds(-1.0),
                release: Seconds(-1.0),
                ..base
            },
            GateControls {
                range: Db(12.0),
                ..base
            },
        ];
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            all.push(GateControls {
                threshold: Db(bad),
                range: Db(bad),
                ..base
            });
            all.push(GateControls {
                attack: Seconds(f64::from(bad)),
                hold: Seconds(f64::from(bad)),
                release: Seconds(f64::from(bad)),
                ..base
            });
        }
        all
    }

    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        &[(
            Hook::Extremes,
            "the follower is a one-pole inside the unit circle for every time and the hold \
             is a finite count: there is no loop to close on its limit",
        )]
    }
}

impl RecurrenceCase for GateFamily {
    fn probes(&self) -> Vec<GateControls> {
        vec![
            controls(-30.0, 0.003, 0.05, 0.08, -40.0),
            controls(-12.0, 0.001, 0.0, 0.05, -12.0),
            controls(-50.0, 0.01, 0.1, 0.2, -80.0),
        ]
    }

    fn extremes(&self) -> Vec<GateControls> {
        Vec::new()
    }

    /// A range that moves (it must glide), times and a hold that move (the follower
    /// smooths them) and a threshold that moves.
    fn steps(&self) -> Vec<(GateControls, GateControls)> {
        vec![
            (
                controls(-30.0, 0.01, 0.05, 0.1, -60.0),
                controls(-30.0, 0.01, 0.05, 0.1, -12.0),
            ),
            (
                controls(-30.0, 0.002, 0.02, 0.05, -40.0),
                controls(-30.0, 0.02, 0.1, 0.3, -40.0),
            ),
            (
                controls(-12.0, 0.01, 0.05, 0.1, -40.0),
                controls(-40.0, 0.01, 0.05, 0.1, -40.0),
            ),
        ]
    }
}

crate::family_contract!(design());
crate::recurrence_contract!(design(), BlockIndependent);

/// The same cases with the detector on the signal: no key bus, so the harness feeds none.
mod own_key {
    use super::*;

    crate::family_contract!(own());
    crate::recurrence_contract!(own(), BlockIndependent);
}

/// A prepared runner on `family` at `c`.
fn runner_on(family: &GateFamily, c: &GateControls) -> Realized<GateRecurrence> {
    built(family, c, RATE).0
}

fn runner(c: &GateControls) -> Realized<GateRecurrence> {
    runner_on(&design(), c)
}

/// `audio` (one slice per channel) through `k`, keyed by `key`, with `lane` as the
/// threshold lane when given.
fn run(
    k: &mut Realized<GateRecurrence>,
    audio: &[&[f32]],
    key: &[&[f32]],
    lane: Option<&[f32]>,
) -> Vec<Vec<f32>> {
    let n = audio[0].len();
    let mut out = vec![vec![0.0f32; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + CHUNK).min(n);
        let a: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
        let kk: Vec<&[f32]> = key.iter().map(|c| &c[at..end]).collect();
        let mut feed = Feed::new(&a).with_key(&kk);
        if let Some(lane) = lane {
            feed = feed.with_lane(0, &lane[at..end]);
        }
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        k.process(&feed, &mut outs, end - at);
        at = end;
    }
    out
}

/// One channel of `audio` and of the key, as the stereo pair the design runs.
fn run_mono(
    k: &mut Realized<GateRecurrence>,
    audio: &[f32],
    key: &[f32],
    lane: Option<&[f32]>,
) -> Vec<f32> {
    run(k, &[audio, audio], &[key, key], lane).remove(0)
}

/// A level of `db` as an amplitude, from the definition.
fn amplitude(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// The member holds the figures its definition gives: the hold is `floor(hold * rate)`
/// frames, the closed gain is the range as an amplitude, the times are floored at 0 and the
/// range is capped at 0 dB, and a non-finite control is the default's.
#[test]
fn the_member_holds_the_figures_of_its_definition() {
    let family = design();
    let m = family.member(&controls(-20.0, 0.001, 0.0102, 0.1, -12.0), RATE);
    // 0.0102 s at 48 kHz is 489.6 frames, floored.
    assert_eq!(m.hold_samples(), 489);
    let want = 10f64.powf(-12.0 / 20.0);
    assert!((f64::from(m.closed_gain().get()) / want - 1.0).abs() < 1e-6);
    assert_eq!(m.threshold(), Db(-20.0));

    let m = family.member(
        &GateControls {
            attack: Seconds(-1.0),
            hold: Seconds(-1.0),
            release: Seconds(-1.0),
            range: Db(6.0),
            ..GateControls::DEFAULT
        },
        RATE,
    );
    assert_eq!(
        (m.attack(), m.hold(), m.release()),
        (Seconds(0.0), Seconds(0.0), Seconds(0.0))
    );
    assert_eq!(m.hold_samples(), 0);
    assert_eq!(m.range(), Db(0.0), "a range over 0 dB reads as 0");

    let m = family.member(
        &GateControls {
            threshold: Db(f32::NAN),
            range: Db(f32::INFINITY),
            ..GateControls::DEFAULT
        },
        RATE,
    );
    assert_eq!(m.threshold(), GateControls::DEFAULT.threshold);
    assert_eq!(m.range(), GateControls::DEFAULT.range);
}

/// A key a hair over the threshold opens the gate and one a hair under leaves it closed
/// (the margin, 0.1 percent, is far outside the rounding of the comparison), on a
/// threshold under and over 0 dB; the member says the same from its definition.
#[test]
fn a_level_either_side_of_the_threshold_opens_or_closes() {
    for threshold in [-20.0f32, 6.0] {
        let edge = f64::from(amplitude(f64::from(threshold)));
        let c = controls(threshold, 0.0, 0.0, 0.0, -30.0);
        let m = design().member(&c, RATE);
        for (level, opens) in [(edge * 1.001, true), (edge * 0.999, false)] {
            assert_eq!(m.opens(Amplitude::new(level as f32)), opens);
            let mut k = runner(&c);
            let key = vec![level as f32; 64];
            let out = run_mono(&mut k, &vec![1.0; 64], &key, None);
            let want = if opens { 1.0 } else { amplitude(-30.0) };
            assert!(
                (out[63] / want - 1.0).abs() < 1e-5,
                "threshold {threshold} dB, level {level}: {} vs {want}",
                out[63]
            );
        }
    }
}

/// The settled gain is 1 open and the range closed: a long loud key, then a long quiet one.
#[test]
fn the_settled_gain_is_one_open_and_the_range_closed() {
    for range in [-80.0f32, -24.0, -6.0] {
        let c = controls(-30.0, 0.002, 0.01, 0.02, range);
        let mut k = runner(&c);
        // 40 000 frames is 40 release time constants: the residue is under 1e-17.
        let n = 40_000;
        let ones = vec![1.0f32; n];
        let out = run_mono(&mut k, &ones, &vec![amplitude(-10.0); n], None);
        // An f32 one-pole stalls within `ulp / c` of its target (about 6e-6 here).
        assert!((out[n - 1] - 1.0).abs() < 2e-5, "open: {}", out[n - 1]);
        let out = run_mono(&mut k, &ones, &vec![amplitude(-60.0); n], None);
        let want = amplitude(f64::from(range));
        assert!(
            (out[n - 1] / want - 1.0).abs() < 1e-5,
            "closed at {range} dB: {} vs {want}",
            out[n - 1]
        );
    }
}

/// The hold keeps an opened gate fully open for exactly `floor(hold * rate)` frames after
/// the key falls under the threshold, and the release (instant here) starts on the next.
#[test]
fn the_hold_keeps_the_gate_open_for_its_whole_length() {
    // 0.0102 s at 48 kHz is 489.6 frames: the count is the floor, 489.
    let c = controls(-20.0, 0.0, 0.0102, 0.0, -40.0);
    let hold = 489;
    let (loud, quiet) = (10, 2000);
    let mut key = vec![amplitude(-6.0); loud];
    key.extend(vec![0.0f32; quiet]);
    let ones = vec![1.0f32; loud + quiet];
    let mut k = runner(&c);
    let out = run_mono(&mut k, &ones, &key, None);
    let closed = amplitude(-40.0);
    for (i, &x) in out.iter().enumerate() {
        let open = i < loud + hold;
        let want = if open { 1.0 } else { closed };
        assert!(
            (x / want - 1.0).abs() < 1e-5,
            "frame {i} ({} after the fall): {x} vs {want}",
            i as i64 - loud as i64
        );
    }
}

/// The follower is a one-pole: after one attack time constant of an open key it has risen
/// to `1 - 1/e` of the way from closed to open, and after one release time constant of a
/// closed one `1/e` of the way is left (the coefficient is `1 - exp(-1 / (tau * rate))`,
/// so the gap after `tau * rate` frames is `e^-1`).
#[test]
fn the_follower_takes_one_time_constant_to_the_one_over_e_point() {
    // 480 and 960 frames are 10 and 20 ms at 48 kHz: the time constants are whole frames.
    let (attack, release) = (480usize, 960usize);
    let range = -40.0f32;
    let r = f64::from(amplitude(f64::from(range)));
    let c = controls(-30.0, 0.01, 0.0, 0.02, range);
    let fraction = |gain: f32| (f64::from(gain) - r) / (1.0 - r);
    let e = std::f64::consts::E;

    let mut k = runner(&c);
    let up = run_mono(
        &mut k,
        &vec![1.0; attack],
        &vec![amplitude(-10.0); attack],
        None,
    );
    let rose = fraction(up[attack - 1]);
    assert!(
        (rose - (1.0 - 1.0 / e)).abs() < 1e-3,
        "{rose} after one attack"
    );

    let mut k = runner(&c);
    let _ = run_mono(
        &mut k,
        &vec![1.0; 20_000],
        &vec![amplitude(-10.0); 20_000],
        None,
    );
    let down = run_mono(&mut k, &vec![1.0; release], &vec![0.0; release], None);
    let left = fraction(down[release - 1]);
    assert!(
        (left - 1.0 / e).abs() < 1e-3,
        "{left} left after one release"
    );
}

/// One linked gain on every channel: the loudest key channel opens the gate, wherever it
/// is, a quiet channel does not close it, and every main channel is multiplied by it.
#[test]
fn the_gain_is_linked_across_the_channels_from_the_loudest_key() {
    let n = 4000;
    let c = controls(-20.0, 0.001, 0.0, 0.05, -40.0);
    let loud = vec![0.9f32; n];
    let quiet = vec![0.001f32; n];
    // Exact binary multiples, so a shared gain is a bit-exact relation.
    let (l, r) = (vec![1.0f32; n], vec![0.25f32; n]);
    let mut gains = Vec::new();
    for key in [[&loud, &quiet], [&quiet, &loud], [&loud, &loud]] {
        let mut k = runner(&c);
        let out = run(&mut k, &[&l, &r], &[key[0], key[1]], None);
        for (i, (&a, &b)) in out[0].iter().zip(&out[1]).enumerate() {
            assert_eq!(b, 0.25 * a, "the gain differs by channel at {i}");
        }
        assert!(out[0][n - 1] > 0.99, "the loud key opened the gate");
        gains.push(out[0].clone());
    }
    assert_eq!(gains[0], gains[1], "the key's channel order moved the gain");
    assert_eq!(
        gains[0], gains[2],
        "a quiet key channel lowered the loudest one's gain"
    );
}

/// A side-chain key drives the detector, not the signal: a quiet signal under a loud key
/// passes whole, a loud signal under a silent key (an unwired key) is closed to the range.
#[test]
fn a_side_chain_key_and_not_the_signal_drives_the_detector() {
    let n = 4000;
    let c = controls(-20.0, 0.001, 0.0, 0.05, -40.0);
    let (quiet, loud, silent) = (vec![0.01f32; n], vec![0.9f32; n], vec![0.0f32; n]);

    let mut k = runner(&c);
    let out = run_mono(&mut k, &quiet, &loud, None);
    assert!(
        (out[n - 1] - 0.01).abs() < 1e-7,
        "a loud key opens the gate"
    );

    let mut k = runner(&c);
    let out = run_mono(&mut k, &loud, &silent, None);
    let want = 0.9 * amplitude(-40.0);
    assert!(
        (out[n - 1] / want - 1.0).abs() < 1e-5,
        "a silent key leaves the gate closed: {} vs {want}",
        out[n - 1]
    );
}

/// An own key is the signal itself: no key bus (the family's channels say so), a loud
/// signal opens its own gate and a quiet one under the threshold stays closed.
#[test]
fn an_own_key_detects_the_signal_itself() {
    assert_eq!(own().channels().key_width(), 0);
    assert_eq!(design().channels().key_width(), 2);
    let n = 4000;
    let c = controls(-20.0, 0.001, 0.0, 0.05, -40.0);
    let loud = vec![0.5f32; n];
    let quiet = vec![0.01f32; n];

    let mut k = runner_on(&own(), &c);
    let out = run(&mut k, &[&loud, &loud], &[], None).remove(0);
    assert!(
        (out[n - 1] - 0.5).abs() < 1e-6,
        "a loud signal opens itself: {}",
        out[n - 1]
    );

    let mut k = runner_on(&own(), &c);
    let out = run(&mut k, &[&quiet, &quiet], &[], None).remove(0);
    let want = 0.01 * amplitude(-40.0);
    assert!(
        (out[n - 1] / want - 1.0).abs() < 1e-5,
        "a quiet signal gates itself: {} vs {want}",
        out[n - 1]
    );
}

/// A threshold lane is read per frame: with the follower instant, each frame is open
/// when the key is at or over that frame's threshold, and a frame that is not finite takes
/// the member's.
#[test]
fn a_threshold_lane_is_read_per_frame() {
    let n = 600;
    let base = -30.0f32;
    let c = controls(base, 0.0, 0.0, 0.0, -40.0);
    let level = -20.0f32;
    let lane: Vec<f32> = (0..n)
        .map(|i| match i {
            0..100 => 0.0,
            100..200 => -40.0,
            200..300 => f32::NAN,
            300..400 => f32::INFINITY,
            400..500 => 10.0,
            _ => f32::NEG_INFINITY,
        })
        .collect();
    let mut k = runner(&c);
    let ones = vec![1.0f32; n];
    let out = run_mono(
        &mut k,
        &ones,
        &vec![amplitude(f64::from(level)); n],
        Some(&lane),
    );
    for (i, &t) in lane.iter().enumerate() {
        let threshold = if t.is_finite() { t } else { base };
        let want = if level >= threshold {
            1.0
        } else {
            amplitude(-40.0)
        };
        assert!(
            (out[i] / want - 1.0).abs() < 1e-5,
            "frame {i}: threshold {t}: {} vs {want}",
            out[i]
        );
    }
}

/// A threshold at or under [`Db::FLOOR`] opens on a silent key (silence is pinned at the
/// floor for the comparison) and one above it does not; the member and the render agree,
/// and a level over a floor-pinned threshold still decides on its own.
#[test]
fn silence_opens_a_threshold_at_or_under_the_floor() {
    for (threshold, opens) in [(-200.0f32, true), (Db::FLOOR.get(), true), (-143.0, false)] {
        let c = controls(threshold, 0.0, 0.0, 0.0, -40.0);
        let m = design().member(&c, RATE);
        assert_eq!(m.silence_opens(), opens, "threshold {threshold}");
        assert_eq!(m.opens(Amplitude::new(0.0)), opens);
        let mut k = runner(&c);
        let out = run_mono(&mut k, &vec![1.0; 64], &vec![0.0; 64], None);
        let want = if opens { 1.0 } else { amplitude(-40.0) };
        assert!(
            (out[63] / want - 1.0).abs() < 1e-5,
            "threshold {threshold}: {} vs {want}",
            out[63]
        );
    }
    // -150 dB is 3.2e-8: a key of 1e-10 (-200 dB) is under it, and one of 1e-7 is over.
    let c = controls(-150.0, 0.0, 0.0, 0.0, -40.0);
    let mut k = runner(&c);
    let out = run_mono(&mut k, &vec![1.0; 64], &vec![1.0e-10; 64], None);
    assert!(out[63] < 0.1, "-200 dB is under a -150 dB threshold");
    let out = run_mono(&mut k, &vec![1.0; 64], &vec![1.0e-7; 64], None);
    assert!(out[63] > 0.99, "-140 dB is over a -150 dB threshold");
}

/// A key of NaN reads as silent (the detector's `max` ignores it): the gate stays closed
/// and finite, and it recovers on the next finite level.
#[test]
fn a_nan_key_leaves_the_gate_closed_and_finite() {
    let n = 600;
    let c = controls(-20.0, 0.001, 0.0, 0.05, -40.0);
    let mut k = runner(&c);
    let ones = vec![1.0f32; n];
    let out = run_mono(&mut k, &ones, &vec![f32::NAN; n], None);
    assert!(out.iter().all(|x| x.is_finite()));
    assert!((out[n - 1] / amplitude(-40.0) - 1.0).abs() < 1e-5);
    let out = run_mono(&mut k, &vec![1.0; 4000], &vec![0.5; 4000], None);
    assert!(out[3999] > 0.99, "it recovers on a finite key");
}

/// A range that moves glides over [`GAIN_GLIDE`] (5 ms: 240 frames at 48 kHz) in a straight
/// line in dB, frame `i` being `(i + 1) / 240` of the way, and is exactly its value after;
/// no frame steps.
#[test]
fn a_moved_range_glides_over_five_milliseconds() {
    let frames = (0.005 * RATE.get()).ceil() as usize;
    assert_eq!(frames, 240);
    let (a, b) = (
        controls(-30.0, 0.0, 0.0, 0.0, -60.0),
        controls(-30.0, 0.0, 0.0, 0.0, -12.0),
    );
    let mut k = runner(&a);
    k.retarget(&design().member(&b, RATE));
    let n = 2 * frames;
    // A closed gate (silent key): the gain is the range itself.
    let out = run_mono(&mut k, &vec![1.0; n], &vec![0.0; n], None);
    let mut previous = f64::from(amplitude(-60.0));
    for (i, &x) in out.iter().enumerate() {
        let db = -60.0 + 48.0 * ((i + 1).min(frames) as f64 / frames as f64);
        let want = 10f64.powf(db / 20.0);
        assert!(
            (f64::from(x) / want - 1.0).abs() < 1e-5,
            "frame {i}: {x} vs {want}"
        );
        // 48 dB over 240 frames is 0.2 dB a frame, a factor of 1.0233.
        assert!(f64::from(x) / previous < 1.025, "a step at {i}");
        previous = f64::from(x);
    }
}

/// A range that does not move is applied exactly from the first frame (a runner built at
/// a member is at it: nothing glides from a default).
#[test]
fn a_held_range_applies_exactly() {
    let c = controls(-30.0, 0.0, 0.0, 0.0, -12.0);
    let mut k = runner(&c);
    let out = run_mono(&mut k, &[1.0; 50], &[0.0; 50], None);
    let want = 10f64.powf(-12.0 / 20.0);
    for &x in &out {
        assert!((f64::from(x) / want - 1.0).abs() < 1e-6, "{x} vs {want}");
    }
}

/// A time that moves lands at once (the follower is the smoother): a release that
/// shortens from 1 s to instant closes the gate on the next frame, with no glide.
#[test]
fn a_moved_time_lands_at_once() {
    let (a, b) = (
        controls(-20.0, 0.0, 0.0, 1.0, -40.0),
        controls(-20.0, 0.0, 0.0, 0.0, -40.0),
    );
    let mut k = runner(&a);
    let _ = run_mono(&mut k, &vec![1.0; 100], &vec![0.5; 100], None);
    k.retarget(&design().member(&b, RATE));
    let out = run_mono(&mut k, &[1.0; 4], &[0.0; 4], None);
    assert!(
        (out[0] / amplitude(-40.0) - 1.0).abs() < 1e-5,
        "closed on the first frame: {}",
        out[0]
    );
}

/// A reset clears the follower and the hold: a key that has just fallen, mid-hold, leaves
/// the gate open before it and closed after it.
#[test]
fn a_reset_clears_the_follower_and_the_hold() {
    let c = controls(-20.0, 0.0, 1.0, 1.0, -40.0);
    let mut k = runner(&c);
    let _ = run_mono(&mut k, &vec![1.0; 100], &vec![0.5; 100], None);
    let out = run_mono(&mut k, &[1.0; 4], &[0.0; 4], None);
    assert!((out[0] - 1.0).abs() < 1e-6, "the hold keeps it open");
    k.reset();
    let out = run_mono(&mut k, &[1.0; 4], &[0.0; 4], None);
    assert!(
        (out[0] / amplitude(-40.0) - 1.0).abs() < 1e-5,
        "a reset gate is closed: {}",
        out[0]
    );
}

/// The per-frame gains are sized from the prepared longest block: a block of exactly that
/// length renders (a default gate passes the signal).
#[test]
fn a_block_of_the_prepared_length_renders() {
    let mut k = design().realized();
    k.prepare(
        &(),
        &Setup {
            rate: RATE,
            max_block: tutti_types::MaxBlock::of(tutti_types::Samples(300)),
        },
    );
    k.retarget(&design().member(&GateControls::DEFAULT, RATE));
    let ones = vec![1.0f32; 300];
    let silent = vec![0.0f32; 300];
    let mut out = [vec![0.0f32; 300], vec![0.0f32; 300]];
    let [l, r] = &mut out;
    k.process(
        &Feed::new(&[&ones, &ones]).with_key(&[&silent, &silent]),
        &mut [l.as_mut_slice(), r.as_mut_slice()],
        300,
    );
    assert!(out[0].iter().all(|&x| x == 1.0));
}
