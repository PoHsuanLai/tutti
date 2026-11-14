use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde::{Deserialize, Serialize};
use tutti_types::{BusTopology, Db, Feedback, Hz, SampleRate, Seconds};

use super::*;
use crate::MaxDelayError;
use crate::contract::{FamilyCase, Figure, Hook, LtiCase, RecurrenceCase, built};
use crate::{Realized, TAIL_FLOOR};

/// A rate at which a time of `n / 1024` seconds is exactly `n` frames, in `f64` and in the
/// `f32` a control is held in.
const BINARY: SampleRate = SampleRate::of(1024.0);
const MAX: MaxDelay = MaxDelay::of(Seconds(1.0));

fn controls(time: f64, feedback: f32, cross: f32) -> DelayControls {
    DelayControls {
        time: Seconds(time),
        feedback: Feedback(feedback),
        cross_feedback: Feedback(cross),
    }
}

fn bus(width: u16) -> BusTopology {
    BusTopology::assumed(width.into()).expect("a test width is one bus")
}

fn mono(mode: InterpolationMode) -> DelayFamily {
    DelayFamily::new(BusTopology::MONO, MAX, mode)
}

/// A stereo delay: the default swap routing, the channels linked in time.
fn stereo() -> DelayFamily {
    DelayFamily::new(BusTopology::STEREO, MAX, InterpolationMode::Linear)
}

/// A stereo delay with no routing between the channels and a time of its own for the right.
fn stereo_apart(right: f64) -> DelayFamily {
    stereo()
        .with_routing(Routing::none(BusTopology::STEREO))
        .unwrap()
        .with_times(ChannelTimes::each(&[Seconds(right)]).unwrap())
        .unwrap()
}

// ---- the contract ---------------------------------------------------------

impl FamilyCase for DelayFamily {
    fn capacity(&self, rate: SampleRate) -> DelayCapacity {
        self.reach(&self.reach_at(rate))
    }

    fn rates(&self) -> Vec<SampleRate> {
        vec![SampleRate::SR_48K, SampleRate::SR_96K, SampleRate::SR_44K1]
    }

    /// The family's own longest delay is the domain it is built to.
    fn reach_at(&self, rate: SampleRate) -> Reach<MaxDelay> {
        Reach::new(rate, self.max_delay(), TAIL_FLOOR).expect("TAIL_FLOOR is a valid floor")
    }

    /// Times from under a frame to the longest, and loop gains up to the stable pair's bound
    /// (the cubic's own bound on `q` is lower: see [`DelayFamily::q_limit`]).
    fn reachable(&self) -> Vec<DelayControls> {
        let limit = self.q_limit();
        let mut all = Vec::new();
        for time in [0.0, 0.000_03, 0.003, 0.0123, 0.05, 0.4, 1.0] {
            for fb in [0.0, 0.3, 0.6, 0.95] {
                for cf in [0.0, 0.2, 0.5] {
                    if fb + cf <= limit {
                        all.push(controls(time, fb, cf));
                    }
                }
            }
        }
        let mut rng = StdRng::seed_from_u64(24);
        all.extend((0..32).map(|_| {
            let fb = rng.random_range(0.0..limit);
            controls(
                rng.random_range(0.0..1.0),
                fb,
                rng.random_range(0.0..limit - fb),
            )
        }));
        all
    }

    /// A time that is not finite, negative or past the longest; and (where the loop's bound
    /// has room for it) a feedback and a cross-feedback that are not finite, negative or past
    /// the stable range: each a member the family can run.
    fn hostile(&self) -> Vec<DelayControls> {
        let base = controls(0.01, 0.5_f32.min(self.q_limit()), 0.0);
        let mut all = Vec::new();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e9] {
            all.push(DelayControls {
                time: Seconds(bad),
                ..base
            });
        }
        if self.q_limit() >= 0.99 {
            for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, 5.0] {
                all.push(DelayControls {
                    feedback: Feedback(bad),
                    cross_feedback: Feedback(0.3),
                    ..base
                });
                all.push(DelayControls {
                    cross_feedback: Feedback(bad),
                    ..base
                });
            }
        }
        all
    }

    /// The first frame an impulse is heard on any channel: the earliest tap of the output
    /// read with a weight (a cubic reads one frame ahead of the delay's whole part).
    fn musical_delay(&self, m: &DelayMember) -> usize {
        (0..m.width())
            .flat_map(|c| {
                let (taps, n) = read_taps(m.delays[c], m.cap, m.interpolation);
                taps.into_iter().take(n).filter(|t| t.1 != 0.0).map(|t| t.0)
            })
            .min()
            .unwrap_or(0)
    }

    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        &[]
    }
}

impl DelayFamily {
    /// The largest loop gain `q` the contract's controls reach. The sufficient bound on the
    /// radius is `q L`, with `L` the interpolator's l1 gain, which is under 1 only below
    /// `q = 0.8` for the cubic Hermite (`L = 1.25`): its cases stay there, and the others
    /// reach the stable pair's whole range.
    fn q_limit(&self) -> f32 {
        if self.interpolation == InterpolationMode::CubicHermite {
            0.75
        } else {
            0.99
        }
    }

    /// A time whose frames are whole at every rate the contract runs at, so the closed-form
    /// figures below are exact.
    const WHOLE: f64 = 0.01;
}

impl RecurrenceCase for DelayFamily {
    /// A comb of tens of milliseconds at a few loop gains, and one of 100 ms at a high one:
    /// the `decay` case reads the slope of the energy in windows of 4096 frames, so it needs
    /// a loop that rings across many of them (every probe has a feedback: a lone echo is
    /// smeared by the measuring windows past the tail it is measured against).
    fn probes(&self) -> Vec<DelayControls> {
        let top = self.q_limit().min(0.9);
        let (slow_fb, slow_cf) = if self.q_limit() < 0.9 {
            (0.7, 0.0)
        } else {
            (0.6, 0.3)
        };
        vec![
            controls(DelayFamily::WHOLE, 0.5, 0.0),
            controls(0.0123, top * 0.8, 0.1),
            controls(0.003, 0.2, 0.3),
            controls(0.1, slow_fb, slow_cf),
        ]
    }

    /// The loop at its greatest gain: the stable pair's bound, or the cubic's.
    fn extremes(&self) -> Vec<DelayControls> {
        let q = self.q_limit();
        vec![
            controls(0.05, q, 0.0),
            controls(0.003, q - 0.2, 0.2),
            controls(1.0, q, 0.0),
        ]
    }

    /// A time that moves (within the slew's reach: the click bound is a ratio of the steady
    /// state's jumps), a feedback that moves and a cross-feedback that moves.
    fn steps(&self) -> Vec<(DelayControls, DelayControls)> {
        vec![
            (controls(0.01, 0.4, 0.0), controls(0.0112, 0.4, 0.0)),
            (controls(0.01, 0.2, 0.1), controls(0.01, 0.6, 0.1)),
            (controls(0.01, 0.4, 0.0), controls(0.01, 0.4, 0.3)),
        ]
    }

    /// The probes with a time of whole frames: a fractional delay's first sound is placed
    /// by `musical_delay` too, but only these have exactly the closed-form figures.
    fn static_probes(&self) -> Vec<DelayControls> {
        self.probes()
    }
}

impl LtiCase for DelayFamily {
    /// The comb of whole frames at its poles' angles: `z^-d` is `+-1` at DC and Nyquist,
    /// so `H = 1 / (1 - g)` or `1 / (1 +- g)` there, from the controls alone. A coupled
    /// pair is driven in common mode (the same signal on both inputs), which excites only
    /// the sum mode, whose loop gain is `g + c`.
    fn figures(&self) -> Vec<Figure<DelayControls>> {
        let (g, c) = (0.5f32, if self.routing.is_none() { 0.0 } else { 0.2 });
        let mut all = Vec::new();
        for rate in [SampleRate::SR_48K, SampleRate::SR_96K] {
            let d = DelayFamily::WHOLE * rate.get();
            let q = f64::from(g + c);
            for (freq, sign) in [
                (0.0, 1.0),
                (rate.get() / 2.0, if d % 2.0 == 0.0 { 1.0 } else { -1.0 }),
            ] {
                all.push(Figure {
                    controls: controls(DelayFamily::WHOLE, g, c),
                    rate,
                    output: 0,
                    freq: Hz(freq as f32),
                    gain: 1.0 / (1.0 - sign * q).abs(),
                    tolerance: 1e-5,
                });
            }
        }
        all
    }

    /// The impulse response of a comb of tens of milliseconds, rendered to its tail.
    fn fill(&self, rate: SampleRate, trips: f32) -> usize {
        (0.02 * f64::from(trips) * rate.get()) as usize
    }

    /// The decay is slowest at DC, where the comb's peaks are.
    fn low_tone(&self, _: SampleRate) -> f32 {
        0.0
    }
}

mod linear_mono {
    use super::*;
    fn design() -> DelayFamily {
        mono(InterpolationMode::Linear)
    }
    crate::family_contract!(design(), Grows, Lti);
    crate::recurrence_contract!(design(), Lti, BlockIndependent);
}

mod whole_sample_mono {
    use super::*;
    fn design() -> DelayFamily {
        mono(InterpolationMode::None)
    }
    crate::family_contract!(design(), Grows, Lti);
    crate::recurrence_contract!(design(), Lti, BlockIndependent);
}

mod cubic_mono {
    use super::*;
    fn design() -> DelayFamily {
        mono(InterpolationMode::CubicHermite)
    }
    crate::family_contract!(design(), Grows, Lti);
    crate::recurrence_contract!(design(), Lti, BlockIndependent);
}

/// Two channels with no routing and a time of its own for the right: the diagonal case.
mod stereo_uncoupled {
    use super::*;
    fn design() -> DelayFamily {
        stereo_apart(0.0123)
    }
    crate::family_contract!(design(), Grows, Lti);
    crate::recurrence_contract!(design(), Lti, BlockIndependent);
}

/// The stereo ping-pong, its channels coupled through the swap routing.
mod stereo_ping_pong {
    use super::*;
    fn design() -> DelayFamily {
        stereo()
    }
    crate::family_contract!(design(), Grows, Lti);
    crate::recurrence_contract!(design(), Lti, BlockIndependent);
}

// ---- the figures of the definition ---------------------------------------

/// `audio` (one slice per channel) through `k` in blocks of `block`, with the time and
/// feedback lanes when given.
fn run_in(
    k: &mut Realized<DelayRecurrence>,
    audio: &[Vec<f32>],
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

fn run(k: &mut Realized<DelayRecurrence>, audio: &[Vec<f32>]) -> Vec<Vec<f32>> {
    run_in(k, audio, [None, None], 64)
}

/// A runner of `family` at `c`, prepared at [`BINARY`] for the contract's longest block.
fn runner(family: &DelayFamily, c: &DelayControls) -> Realized<DelayRecurrence> {
    built(family, c, BINARY).0
}

fn impulse(len: usize) -> Vec<f32> {
    let mut x = vec![0.0; len];
    x[0] = 1.0;
    x
}

/// An impulse is heard exactly `d` frames later, in each interpolation mode, at every
/// whole delay: the line is `d` frames long and nothing else is heard.
#[test]
fn an_impulse_arrives_at_exactly_the_delay() {
    for mode in [
        InterpolationMode::None,
        InterpolationMode::Linear,
        InterpolationMode::CubicHermite,
    ] {
        for d in [1usize, 2, 3, 16, 100, 513] {
            let mut k = runner(&mono(mode), &controls(d as f64 / 1024.0, 0.0, 0.0));
            let y = run(&mut k, &[impulse(d + 40)]).remove(0);
            for (i, &s) in y.iter().enumerate() {
                let want = f32::from(i == d);
                assert_eq!(s, want, "{mode:?}, delay {d}, frame {i}");
            }
        }
    }
}

/// A delay of `k + f` frames blends the two frames around it: `1 - f` of the impulse at
/// `k` and `f` of it at `k + 1`, for the linear read; and the half frame of a delay under
/// one frame blends the impulse with the silence before it.
#[test]
fn a_fractional_delay_blends_the_frames_around_it_by_the_fraction() {
    for (d, at, f) in [
        (10.25f64, 10usize, 0.25f32),
        (7.5, 7, 0.5),
        (3.875, 3, 0.875),
        (0.5, 0, 0.5),
    ] {
        let mut k = runner(
            &mono(InterpolationMode::Linear),
            &controls(d / 1024.0, 0.0, 0.0),
        );
        let y = run(&mut k, &[impulse(40)]).remove(0);
        for (i, &s) in y.iter().enumerate() {
            let want = if i == at {
                1.0 - f
            } else if i == at + 1 {
                f
            } else {
                0.0
            };
            assert_eq!(s, want, "delay {d}, frame {i}");
        }
    }
}

/// With feedback `g` and a whole delay of `D` frames the impulse response is `g^k` at
/// `(k + 1) D` and silence between: each echo is the last one scaled by the feedback, a
/// period on.
#[test]
fn feedback_decays_by_the_gain_each_trip() {
    let (d, g) = (16usize, 0.6f32);
    let mut k = runner(
        &mono(InterpolationMode::Linear),
        &controls(d as f64 / 1024.0, g, 0.0),
    );
    let y = run(&mut k, &[impulse(d * 12 + 5)]).remove(0);
    for (i, &s) in y.iter().enumerate() {
        let want = if i >= d && i % d == 0 {
            g.powi((i / d - 1) as i32)
        } else {
            0.0
        };
        assert!((s - want).abs() < 1e-6, "frame {i}: {s}, want {want}");
    }
}

/// The stereo ping-pong from the definition. An impulse in the left of equal delays `D`
/// splits into the common mode, which recirculates with the gain `g + c`, and the
/// differential mode, with `g - c` (the swap routing adds the other channel's tap): so the
/// left is heard at `(n + 1) D` with `((g + c)^n + (g - c)^n) / 2` and the right with
/// `((g + c)^n - (g - c)^n) / 2`.
#[test]
fn a_ping_pong_splits_into_a_common_and_a_differential_mode() {
    let (d, g, c) = (8usize, 0.3f32, 0.4f32);
    let mut k = runner(&stereo(), &controls(d as f64 / 1024.0, g, c));
    let len = d * 10 + 3;
    let y = run(&mut k, &[impulse(len), vec![0.0; len]]);
    for n in 0..9usize {
        let (sum, diff) = (
            f64::from(g + c).powi(n as i32),
            f64::from(g - c).powi(n as i32),
        );
        for (ch, want) in [(0, (sum + diff) / 2.0), (1, (sum - diff) / 2.0)] {
            let got = f64::from(y[ch][(n + 1) * d]);
            assert!(
                (got - want).abs() < 1e-6,
                "channel {ch}, echo {n}: {got}, want {want}"
            );
        }
    }
    for (ch, channel) in y.iter().enumerate() {
        for (i, &s) in channel.iter().enumerate() {
            if i % d != 0 || i == 0 {
                assert_eq!(
                    s, 0.0,
                    "channel {ch}, frame {i}: nothing between the echoes"
                );
            }
        }
    }
}

/// With no routing the cross-feedback is inert, and a channel's time past the first is
/// its own when the channels are not linked and channel 0's when they are: the impulse of
/// each channel comes back at its own delay.
#[test]
fn channel_times_are_linked_or_each_their_own() {
    let both = vec![impulse(60), impulse(60)];
    let heard = |y: &[Vec<f32>]| {
        y.iter()
            .map(|c| c.iter().position(|&s| s != 0.0).unwrap())
            .collect::<Vec<_>>()
    };
    // Each its own: the right at 40 frames, the left at 10; the cross-feedback is inert.
    let mut k = runner(
        &stereo_apart(40.0 / 1024.0),
        &controls(10.0 / 1024.0, 0.0, 0.5),
    );
    assert_eq!(heard(&run(&mut k, &both)), [10, 40]);
    // Linked: every channel follows the control.
    let linked = stereo()
        .with_routing(Routing::none(BusTopology::STEREO))
        .unwrap();
    let mut k = runner(&linked, &controls(10.0 / 1024.0, 0.0, 0.0));
    assert_eq!(heard(&run(&mut k, &both)), [10, 10]);
    // Six channels with no routing never recirculate across channels, whatever the
    // cross-feedback says: an impulse in channel 3 is heard on channel 3 alone.
    let wide = DelayFamily::new(bus(6), MAX, InterpolationMode::Linear);
    let mut k = runner(&wide, &controls(10.0 / 1024.0, 0.0, 0.9));
    let mut x = vec![vec![0.0; 60]; 6];
    x[3] = impulse(60);
    let y = run(&mut k, &x);
    for (ch, c) in y.iter().enumerate() {
        assert_eq!(c.iter().any(|&s| s != 0.0), ch == 3, "channel {ch}");
    }
}

/// A delay moves no faster than [`DELAY_SLEW`] frames of delay per frame, however far its
/// time is moved, and arrives exactly on the time. A linear read of a ramp is exact, so
/// the delay in use at frame `n` is `n - y[n]`, read off the output: a time moved from 100
/// to 400 frames glides for `300 / 0.125 = 2400` frames, never faster, and holds there.
#[test]
fn a_time_glide_is_bounded_by_the_slew_and_lands_exactly() {
    let family = mono(InterpolationMode::Linear);
    let mut k = built(
        &family,
        &controls(100.0 / 1000.0, 0.0, 0.0),
        SampleRate::of(1000.0),
    )
    .0;
    k.retarget(&family.member(&controls(400.0 / 1000.0, 0.0, 0.0), SampleRate::of(1000.0)));
    let len = 4_000;
    let ramp: Vec<f32> = (0..len).map(|n| n as f32).collect();
    let y = run(&mut k, &[ramp]).remove(0);
    let delay: Vec<f64> = (500..len).map(|n| n as f64 - f64::from(y[n])).collect();
    let worst = delay
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f64::max);
    assert!(
        worst <= f64::from(DELAY_SLEW) + 2e-3,
        "the delay moved {worst} frames in one"
    );
    assert!(
        delay.windows(2).all(|w| w[1] >= w[0] - 2e-3),
        "the glide is monotone"
    );
    // At frame `n` the glide has taken `n + 1` steps of the 2400: still short of 400 until
    // the last, and on it from then on.
    let at = |n: usize| delay[n - 500];
    assert!(
        (at(1000) - (100.0 + 300.0 * 1001.0 / 2400.0)).abs() < 2e-3,
        "{}",
        at(1000)
    );
    assert!(
        (at(2399) - 400.0).abs() < 2e-3,
        "arrived on the time: {}",
        at(2399)
    );
    assert!((at(3999) - 400.0).abs() < 2e-3, "and held: {}", at(3999));
}

/// A time lane, in seconds, is the delay of **every frame**: a ramp through a delay swept
/// by a sine (a flanger's) is the ramp at the lane's time, `y[n] = n - tau[n]`, read off
/// the output frame by frame; and a frame that is not finite, or not a time, is the
/// member's glided delay (here 20 frames), and one past the longest delay is the longest.
#[test]
fn a_time_lane_is_the_delay_of_each_frame() {
    let family = mono(InterpolationMode::Linear);
    let len = 4_000;
    let tau = |n: usize| -> f32 {
        match n % 500 {
            100 => f32::NAN,
            200 => f32::INFINITY,
            300 => 1e9,
            _ => (40.0 + 25.0 * (n as f32 * 0.01).sin()) / 1024.0,
        }
    };
    let lane: Vec<f32> = (0..len).map(tau).collect();
    let ramp: Vec<f32> = (0..len).map(|n| n as f32).collect();
    let mut k = runner(&family, &controls(20.0 / 1024.0, 0.0, 0.0));
    let y = run_in(&mut k, &[ramp], [Some(&lane), None], 64).remove(0);
    for n in 1_200..len {
        let want = match n % 500 {
            100 | 200 => 20.0,
            // Past the longest delay: the longest (1024 frames), which a ramp of fewer
            // frames has not reached: the line holds only the zeros from before the start.
            300 => 1024.0,
            _ => f64::from(lane[n]) * 1024.0,
        };
        let got = n as f64 - f64::from(y[n]);
        if n % 500 == 300 {
            continue;
        }
        assert!(
            (got - want).abs() < 2e-3,
            "frame {n}: delay {got}, want {want}"
        );
    }
    // The longest: an impulse swept to 1e9 s is heard 1024 frames on, not before and not
    // never.
    let mut x = impulse(1_300);
    x[0] = 1.0;
    let lane = vec![1e9f32; 1_300];
    let mut k = runner(&family, &controls(20.0 / 1024.0, 0.0, 0.0));
    let y = run_in(&mut k, &[x], [Some(&lane), None], 64).remove(0);
    assert_eq!(y.iter().position(|&s| s != 0.0), Some(1024));
}

/// A feedback lane is the feedback of every frame: held at `g` it is the echo train of a
/// cell at `g`; a frame that is not finite is the glided cell's (here 0.5); and a value
/// past the stable range is the range's last (0.99).
#[test]
fn a_feedback_lane_is_the_feedback_of_each_frame() {
    let (d, len) = (16usize, 16 * 8 + 5);
    let family = mono(InterpolationMode::Linear);
    let train = |g: f32, k: usize| g.powi(k as i32);
    for (lane, g) in [
        (0.8f32, 0.8f32),
        (f32::NAN, 0.5),
        (f32::INFINITY, 0.5),
        (5.0, 0.99),
        (-2.0, 0.0),
    ] {
        let lane = vec![lane; len];
        let mut k = runner(&family, &controls(d as f64 / 1024.0, 0.5, 0.0));
        let y = run_in(&mut k, &[impulse(len)], [None, Some(&lane)], 64).remove(0);
        for (i, &s) in y.iter().enumerate() {
            let want = if i >= d && i % d == 0 && (i / d == 1 || g > 0.0) {
                train(g, i / d - 1)
            } else {
                0.0
            };
            assert!(
                (s - want).abs() < 1e-5,
                "lane gives {g}, frame {i}: {s}, want {want}"
            );
        }
    }
}

/// A feedback cell that moves glides over [`GAIN_GLIDE`](crate::GAIN_GLIDE) (5 ms: 240
/// frames at 48 kHz), counted on the stream. The first echo has the feedback of its own
/// frame: an echo at `2 D` is the feedback `D + 1` steps into the glide, so `D = 100` of
/// the 240 steps from 0.2 to 0.8 is `0.2 + 0.6 * 101 / 240`, and one past the glide, `D = 300`, is 0.8.
#[test]
fn a_feedback_glides_over_five_milliseconds_counted_on_the_stream() {
    let rate = SampleRate::SR_48K;
    let family = mono(InterpolationMode::Linear);
    for (d, want) in [(100usize, 0.2 + 0.6 * 101.0 / 240.0), (300, 0.8)] {
        let c = |fb| controls(d as f64 / rate.get(), fb, 0.0);
        let mut k = built(&family, &c(0.2), rate).0;
        k.retarget(&family.member(&c(0.8), rate));
        let y = run(&mut k, &[impulse(2 * d + 8)]).remove(0);
        let got = f64::from(y[2 * d]);
        assert!((got - want).abs() < 1e-5, "D {d}: {got}, want {want}");
    }
}

// The shape `check_range_contract` checks is serde's; `MaxDelay` has none in the library
// (nothing persists it), so the check runs on the same `try_from` shape written here.
impl Serialize for MaxDelay {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.get().serialize(s)
    }
}

impl<'de> Deserialize<'de> for MaxDelay {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::try_from(Seconds::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// `MaxDelay` keeps the range-checked shape: `new` refuses what is not a longest delay (not
/// finite, not above zero, past the cap: a finite 1e30 s among them) and never clamps.
#[test]
fn a_max_delay_keeps_the_range_checked_shape() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: MaxDelay::new,
        get: MaxDelay::get,
        of: Some(MaxDelay::of),
        valid: &[Seconds(1e-9), Seconds(0.1), Seconds(2.0), MaxDelay::CAP],
        invalid: &[
            Seconds(0.0),
            Seconds(-0.1),
            Seconds(10.000_001),
            Seconds(1e30),
            Seconds(f64::NAN),
            Seconds(f64::INFINITY),
            Seconds(f64::NEG_INFINITY),
        ],
    });
    assert_eq!(
        MaxDelay::try_from(Seconds(-1.0)),
        Err(MaxDelayError(Seconds(-1.0)))
    );
}

/// A line holds `ceil(max * rate)` frames, from the exact product (0.1 s at 48 kHz is
/// 4800, where the `f32` nearest 0.1 would make 4801), and never more than the guard
/// against an absurd rate.
#[test]
fn a_max_delay_is_a_line_of_the_exact_ceiling_of_its_frames() {
    let max = MaxDelay::of(Seconds(0.1));
    assert_eq!(max.frames(SampleRate::SR_48K), 4800);
    assert_eq!(max.frames(SampleRate::SR_44K1), 4410);
    assert_eq!(
        MaxDelay::of(MaxDelay::CAP).frames(SampleRate::SR_192K),
        1_920_000
    );
    assert_eq!(
        max.frames(SampleRate::of(1e15)),
        super::super::max_delay::MAX_FRAMES
    );
}

/// A routing refuses what it cannot hold: a matrix of the wrong length, a weight that is
/// not finite (it would survive the scaling, `inf / inf` being NaN, and poison the loop
/// for good) and a row that sums past 1; and it keeps what it is given.
#[test]
fn a_routing_refuses_what_it_cannot_hold() {
    let two = BusTopology::STEREO;
    assert_eq!(
        Routing::new(two, &[0.0, 1.0, 1.0]),
        Err(RoutingError::Length {
            width: 2,
            expected: 4,
            got: 3
        })
    );
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let e = Routing::new(two, &[0.0, bad, 1.0, 0.0]).unwrap_err();
        assert!(
            matches!(
                e,
                RoutingError::NotFinite {
                    row: 0,
                    column: 1,
                    ..
                }
            ),
            "{e:?}"
        );
        assert!(Routing::normalized(two, &[0.0, bad, 1.0, 0.0]).is_err());
    }
    assert_eq!(
        Routing::new(two, &[0.0, 1.0, 0.7, 0.4]),
        Err(RoutingError::Row { row: 1, sum: 1.1 })
    );
    // A row that sums to exactly 1, and a negative weight, are held.
    let r = Routing::new(two, &[0.5, -0.5, 1.0, 0.0]).unwrap();
    assert_eq!(
        [
            r.weight(0, 0),
            r.weight(0, 1),
            r.weight(1, 0),
            r.weight(1, 1)
        ],
        [0.5, -0.5, 1.0, 0.0]
    );
    assert_eq!(r.weight(9, 9), 0.0, "a channel off the bus has no weight");
}

/// `normalized` is the one saturation: a row past 1 is scaled down to sum to 1 with its
/// weights' ratios kept, and a row within 1 is left alone.
#[test]
fn a_normalized_routing_scales_a_row_past_one_and_no_other() {
    let four = bus(4);
    let r = Routing::normalized(four, &[1.0; 16]).unwrap();
    for row in 0..4 {
        assert!(
            (r.row_sum(row) - 1.0).abs() < 1e-6,
            "row {row}: {}",
            r.row_sum(row)
        );
        assert!((0..4).all(|c| r.weight(row, c) == 0.25));
    }
    let mut m = [0.0; 16];
    m[1] = 0.6;
    m[2] = -0.2;
    let r = Routing::normalized(four, &m).unwrap();
    assert_eq!((r.weight(0, 1), r.weight(0, 2)), (0.6, -0.2));
}

/// The presets: the swap crosses left and right, a ring feeds each channel from the one
/// before it (the first from the last), none is silent, and a mono ring has nobody to feed
/// from.
#[test]
fn the_routing_presets_are_what_their_names_say() {
    let swap = Routing::swap();
    assert_eq!(
        [
            swap.weight(0, 1),
            swap.weight(1, 0),
            swap.weight(0, 0),
            swap.weight(1, 1)
        ],
        [1.0, 1.0, 0.0, 0.0]
    );
    let ring = Routing::ring(bus(4));
    for c in 0..4 {
        for j in 0..4 {
            assert_eq!(ring.weight(c, j), f32::from(j == (c + 3) % 4), "({c}, {j})");
        }
    }
    assert!(Routing::none(bus(3)).is_none() && !ring.is_none());
    assert!(Routing::ring(BusTopology::MONO).is_none());
    assert_eq!(Routing::ring(BusTopology::STEREO), Routing::swap());
}

/// A routing is for one bus, and the family says so: a routing for another width is
/// refused, and a family's default is the swap at width 2 and none at any other.
#[test]
fn a_family_takes_a_routing_for_its_own_bus_and_defaults_by_its_width() {
    assert_eq!(
        stereo().with_routing(Routing::ring(bus(3))).unwrap_err(),
        RoutingError::Width {
            routing: 3,
            family: 2
        }
    );
    assert_eq!(stereo().routing, Routing::swap());
    for w in [1u16, 3, 6] {
        let f = DelayFamily::new(bus(w), MAX, InterpolationMode::Linear);
        assert!(f.routing.is_none(), "width {w}");
        assert!(f.times().is_linked(), "width {w}");
    }
    // The stereo default is the swap whatever the interpolation or maximum.
    let f = DelayFamily::new(
        BusTopology::STEREO,
        MaxDelay::DEFAULT,
        InterpolationMode::None,
    );
    assert_eq!(f.routing, Routing::swap());
}

/// Channel times refuse a time that is not one, and the family refuses a set that does not
/// fit it: a count that is not one for each channel past the first, and a time past the
/// longest delay.
#[test]
fn channel_times_refuse_what_is_not_a_time_and_what_does_not_fit() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.001] {
        assert!(
            matches!(
                ChannelTimes::each(&[Seconds(0.1), Seconds(bad)]),
                Err(ChannelTimesError::NotATime { channel: 2, .. })
            ),
            "{bad}"
        );
    }
    assert!(matches!(
        ChannelTimes::each(&[Seconds(0.0); 16]),
        Err(ChannelTimesError::TooMany { count: 16, max: 15 })
    ));
    let times = ChannelTimes::each(&[Seconds(0.1), Seconds(0.2)]).unwrap();
    assert_eq!(times.fixed(), [Seconds(0.1), Seconds(0.2)]);
    assert!(!times.is_linked() && ChannelTimes::linked().is_linked());
    assert_eq!(
        stereo().with_times(times).unwrap_err(),
        ChannelTimesError::Length {
            width: 2,
            expected: 1,
            got: 2
        }
    );
    assert!(stereo().with_times(ChannelTimes::linked()).is_ok());
    let past = ChannelTimes::each(&[Seconds(1.5)]).unwrap();
    assert_eq!(
        stereo().with_times(past).unwrap_err(),
        ChannelTimesError::Past {
            channel: 1,
            time: Seconds(1.5),
            max: Seconds(1.0)
        }
    );
    assert!(
        stereo()
            .with_times(ChannelTimes::each(&[Seconds(1.0)]).unwrap())
            .is_ok(),
        "the longest is held"
    );
}

/// The cubic read's taps are a partition of unity and their l1 norm is the bound `1.25`
/// the member's norms use, at every fraction: so the cubic delay's `l1_norm` is not under
/// the truth.
#[test]
fn the_cubic_taps_sum_to_one_and_are_bounded_by_the_l1_gain() {
    for i in 0..=1000 {
        let delay = 10.0 + i as f32 / 1000.0;
        let (taps, n) = read_taps(delay, 100, InterpolationMode::CubicHermite);
        let sum: f32 = taps[..n].iter().map(|t| t.1).sum();
        let l1: f32 = taps[..n].iter().map(|t| t.1.abs()).sum();
        assert!((sum - 1.0).abs() < 1e-5, "delay {delay}: sum {sum}");
        assert!(
            f64::from(l1) <= interpolation_gain(InterpolationMode::CubicHermite) + 1e-6,
            "delay {delay}: {l1}"
        );
    }
}

/// The member's tail is the echoes still to come: `time * (1 + trips)` plus the
/// interpolator's spread, to the floor under the first, and the latency is zero whatever
/// the delay time ("musical delay is not latency").
#[test]
fn the_member_declares_its_echoes_as_the_tail_and_no_latency() {
    let rate = SampleRate::SR_48K;
    let f = mono(InterpolationMode::Linear);
    // No feedback: the first echo is the whole ring, a delay of 480 frames plus the spread.
    let m = f.member(&controls(0.01, 0.0, 0.0), rate);
    assert_eq!(m.latency().get(), 0, "musical delay is not latency");
    let Ring::Finite(t) = m.tail(Db(-60.0)) else {
        panic!("finite")
    };
    assert!((t.get() - 482.0 / 48_000.0).abs() < 1e-12, "{t:?}");
    // At g = 0.5 the echoes are 0.5^k: the -60 dB one is the 10th (0.5^10 < 1e-3), so
    // eleven periods of the delay and the spread.
    let m = f.member(&controls(0.01, 0.5, 0.0), rate);
    let Ring::Finite(t) = m.tail(Db(-60.0)) else {
        panic!("finite")
    };
    assert!((t.get() - 11.0 * 482.0 / 48_000.0).abs() < 1e-9, "{t:?}");
    // A delay of 0 is a one-frame loop, and its blank is the default's.
    assert_eq!(f.blank(rate).delays[0], 0.0);
}

/// A time past the longest, or not a time, is a member the family can run: it reads as the
/// longest, and as the default's.
#[test]
fn a_hostile_time_is_a_member_within_the_line() {
    let rate = SampleRate::SR_48K;
    let f = mono(InterpolationMode::Linear);
    let cap = MAX.frames(rate) as f32;
    for (time, want) in [
        (1e9, cap),
        (-5.0, 0.0),
        (f64::NAN, 0.0),
        (f64::INFINITY, 0.0),
        (0.5, 24_000.0),
    ] {
        let m = f.member(&controls(time, 0.0, 0.0), rate);
        assert_eq!(m.delays[0], want, "{time}");
    }
    // The feedback is the pair's, bounded (the raw ask is kept, and the bound read from it).
    let m = f.member(&controls(0.01, 5.0, 0.0), rate);
    assert_eq!(m.gains().0, Feedback::MAX_STABLE);
    let m = f.member(&controls(0.01, f32::NAN, 0.0), rate);
    assert_eq!(m.gains().0, Feedback::NONE, "not finite is the default's");
}
