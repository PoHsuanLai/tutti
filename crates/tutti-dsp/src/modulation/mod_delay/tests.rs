use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{BusTopology, Db, Feedback, Hz, Phase, PhaseIncrement, SampleRate, Seconds};

use super::*;
use crate::contract::{FamilyCase, Figure, Hook, LtiCase, RecurrenceCase, built, setup};
use crate::{Realized, SweepError, TAIL_FLOOR};

const RATE: SampleRate = SampleRate::SR_48K;

fn controls(rate: f32, depth: f64, feedback: f32) -> ModDelayControls {
    ModDelayControls {
        rate: Hz(rate),
        depth: Seconds(depth),
        feedback: Feedback(feedback),
    }
}

fn bus(width: u16) -> BusTopology {
    BusTopology::assumed(width.into()).expect("a test width is one bus")
}

fn step(x: f32) -> Stagger {
    Stagger::of_step(PhaseIncrement(x))
}

fn family(width: u16, sweep: Sweep, stagger: Stagger) -> ModDelayFamily {
    ModDelayFamily::new(bus(width), sweep, stagger).expect("a stagger that fits")
}

/// A flanger's numbers (1 ms base, a 20 ms line) with a depth that may be zero: the real
/// flanger's least depth is 0.1 ms, so it is never frozen.
fn short_base() -> Sweep {
    Sweep::of(
        Seconds(0.001),
        MaxDelay::of(Seconds(0.02)),
        Seconds(0.0),
        Seconds(0.01),
        Feedback::MAX_STABLE,
    )
}

// ---- the contract ---------------------------------------------------------

impl FamilyCase for ModDelayFamily {
    fn capacity(&self, rate: SampleRate) -> ModDelayCapacity {
        self.reach(&self.reach_at(rate))
    }

    /// The family's own longest delay is the domain it is built to.
    fn reach_at(&self, rate: SampleRate) -> Reach<MaxDelay> {
        Reach::new(rate, self.max_delay(), TAIL_FLOOR).expect("TAIL_FLOOR is a valid floor")
    }

    /// Rates from the floor to audio rate, depths from none to the sweep's greatest, and
    /// feedbacks up to its most.
    fn reachable(&self) -> Vec<ModDelayControls> {
        let s = self.sweep();
        let (lo, hi) = (s.depth_min().get(), s.depth_max().get());
        let fmax = s.feedback_max().get();
        let mut all = Vec::new();
        for rate in [0.01f32, 0.3, 1.0, 5.0, 20.0, 400.0] {
            for depth in [0.0, lo, 0.5 * (lo + hi), hi] {
                for fb in [0.0, 0.3 * fmax, fmax] {
                    all.push(controls(rate, depth, fb));
                }
            }
        }
        let mut rng = StdRng::seed_from_u64(12);
        all.extend((0..32).map(|_| {
            controls(
                rng.gen_range(0.01..30.0),
                rng.gen_range(0.0..=hi),
                rng.gen_range(0.0..=fmax),
            )
        }));
        all
    }

    /// A rate, depth or feedback that is not finite, negative or far past its range: each
    /// a member the family can run.
    fn hostile(&self) -> Vec<ModDelayControls> {
        let base = controls(1.0, self.sweep().depth_max().get() * 0.5, 0.3);
        let mut all = Vec::new();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, 0.0, 1e9] {
            all.push(ModDelayControls {
                rate: Hz(bad),
                ..base
            });
            all.push(ModDelayControls {
                feedback: Feedback(bad),
                ..base
            });
        }
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e9, 1e300] {
            all.push(ModDelayControls {
                depth: Seconds(bad),
                ..base
            });
        }
        all
    }

    /// The first frame an impulse is heard: the earliest tap of the output read at the
    /// centre delay with a weight.
    fn musical_delay(&self, m: &ModDelayMember) -> usize {
        let (taps, n) = read_taps(m.centre_frames(), m.cap, READ);
        taps.into_iter()
            .take(n)
            .filter(|t| t.1 != 0.0)
            .map(|t| t.0)
            .min()
            .unwrap_or(0)
    }

    fn vacant(&self) -> &'static [(Hook, &'static str)] {
        &[]
    }
}

impl RecurrenceCase for ModDelayFamily {
    /// The sweep **moving**, at a few rates, depths and feedbacks: block independence,
    /// reset and the tail run over what the render does.
    fn probes(&self) -> Vec<ModDelayControls> {
        let s = self.sweep();
        let (hi, fmax) = (s.depth_max().get(), s.feedback_max().get());
        vec![
            controls(1.0, 0.25 * hi, 0.5f32.min(fmax)),
            controls(7.0, 0.9 * hi, 0.3f32.min(fmax)),
            controls(0.3, 0.5 * hi, 0.8 * fmax),
        ]
    }

    /// The loop at its greatest gain, swept fast.
    fn extremes(&self) -> Vec<ModDelayControls> {
        let s = self.sweep();
        let (hi, fmax) = (s.depth_max().get(), s.feedback_max().get());
        vec![
            controls(5.0, hi, fmax),
            controls(20.0, 0.5 * hi, 0.9 * fmax),
            controls(0.05, hi, fmax),
        ]
    }

    /// A depth that moves (within the slew's reach: the click bound is a ratio of the
    /// steady states' jumps), a rate that moves and a feedback that moves.
    fn steps(&self) -> Vec<(ModDelayControls, ModDelayControls)> {
        let s = self.sweep();
        let (hi, fmax) = (s.depth_max().get(), s.feedback_max().get());
        vec![
            (
                controls(1.0, 0.25 * hi, 0.4 * fmax),
                controls(1.0, 0.75 * hi, 0.4 * fmax),
            ),
            (
                controls(1.0, 0.5 * hi, 0.4 * fmax),
                controls(3.0, 0.5 * hi, 0.4 * fmax),
            ),
            (
                controls(1.0, 0.5 * hi, 0.2 * fmax),
                controls(1.0, 0.5 * hi, 0.8 * fmax),
            ),
        ]
    }

    /// The sweep **frozen**: a swept delay has no fixed first echo, so the arrival, the
    /// response and the decay are measured at no depth (a sweep whose least depth is above
    /// zero has none, and is not run here).
    fn static_probes(&self) -> Vec<ModDelayControls> {
        let fmax = self.sweep().feedback_max().get();
        vec![
            controls(1.0, 0.0, 0.5f32.min(fmax)),
            controls(5.0, 0.0, 0.9 * fmax),
            // The slowest loop the sweep allows: the `decay` case needs a ring that spans
            // many windows, which a short comb has only at its greatest feedback.
            controls(0.2, 0.0, fmax),
        ]
    }
}

impl LtiCase for ModDelayFamily {
    /// The comb of a whole delay at its poles' angles: `z^-d` is `+-1` at DC and Nyquist, so
    /// `H = 1 / (1 - g)` at DC and `1 / (1 +- g)` at Nyquist, from the controls alone (the
    /// loop is `g z^-d`: a Nyquist of `(-1)^d`). A delay of an odd number of frames is
    /// `1 / (1 + g)` there. The base is a whole number of frames at these rates
    /// to an `f32`'s rounding, which the tolerance covers.
    fn figures(&self) -> Vec<Figure<ModDelayControls>> {
        let g = 0.5f32.min(self.sweep().feedback_max().get());
        let c = controls(1.0, 0.0, g);
        let mut all = Vec::new();
        for rate in [SampleRate::SR_48K, SampleRate::SR_96K] {
            let d = self.member(&c, rate).centre_frames().round() as u64;
            let q = f64::from(g);
            for (freq, sign) in [
                (0.0, 1.0),
                (
                    rate.get() / 2.0,
                    if d.is_multiple_of(2) { 1.0 } else { -1.0 },
                ),
            ] {
                all.push(Figure {
                    controls: c,
                    rate,
                    output: 0,
                    freq: Hz(freq as f32),
                    gain: 1.0 / (1.0 - sign * q).abs(),
                    tolerance: 2e-4,
                });
            }
        }
        all
    }

    /// The decay is slowest at DC, where the comb's peaks are.
    fn low_tone(&self, _: SampleRate) -> f32 {
        0.0
    }
}

mod chorus_mono {
    use super::*;
    fn design() -> ModDelayFamily {
        ModDelayFamily::chorus(BusTopology::MONO)
    }
    crate::family_contract!(design(), Grows, Lti);
    crate::recurrence_contract!(design(), Lti, BlockIndependent);
}

/// Two channels a quarter cycle apart: the diagonal case, each channel its own line.
mod chorus_stereo {
    use super::*;
    fn design() -> ModDelayFamily {
        ModDelayFamily::chorus(BusTopology::STEREO)
    }
    crate::family_contract!(design(), Grows, Lti);
    crate::recurrence_contract!(design(), Lti, BlockIndependent);
}

/// Six channels, each its own sweep.
mod chorus_six {
    use super::*;
    fn design() -> ModDelayFamily {
        ModDelayFamily::chorus(bus(6))
    }
    crate::family_contract!(design(), Grows, Lti);
    crate::recurrence_contract!(design(), Lti, BlockIndependent);
}

/// The flanger's numbers (a short base) with a sweep that may be frozen.
mod short_base_stereo {
    use super::*;
    fn design() -> ModDelayFamily {
        family(2, short_base(), step(0.5))
    }
    crate::family_contract!(design(), Grows, Lti);
    crate::recurrence_contract!(design(), Lti, BlockIndependent);
}

/// The General MIDI chorus: no feedback at all, a 2 ms base in a 5 ms line.
mod general_midi {
    use super::*;
    fn design() -> ModDelayFamily {
        ModDelayFamily::general_midi_chorus(BusTopology::STEREO)
    }
    crate::family_contract!(design(), Grows, Lti);
    crate::recurrence_contract!(design(), Lti, BlockIndependent);
}

/// The real flanger preset: its least depth is above zero, so its sweep is never frozen
/// and only the family's own cases (every member is runnable and inside its line) apply.
mod flanger_preset {
    use super::*;
    fn design() -> ModDelayFamily {
        ModDelayFamily::flanger(BusTopology::STEREO)
    }
    crate::family_contract!(design(), Grows);
}

// ---- the figures of the definition ---------------------------------------

/// `audio` (one slice per channel) through `k` in blocks of `block`.
fn run(k: &mut Realized<ModDelayRecurrence>, audio: &[Vec<f32>], block: usize) -> Vec<Vec<f32>> {
    let n = audio[0].len();
    let mut out = vec![vec![0.0f32; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let a: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
        let feed = Feed::new(&a);
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        k.process(&feed, &mut outs, end - at);
        at = end;
    }
    out
}

/// The sweep of the trajectory tests: a mono, feedback-free sweep, base 20 ms, up to 40 ms
/// deep in an 80 ms line. At 20 Hz and 10 ms it is the worst case for the interpolation
/// between control points.
fn trajectory() -> ModDelayFamily {
    family(
        1,
        Sweep::of(
            Seconds(0.02),
            MaxDelay::of(Seconds(0.08)),
            Seconds(0.0),
            Seconds(0.04),
            Feedback::MAX_STABLE,
        ),
        step(0.0),
    )
}

const SR: f64 = 48_000.0;
const BASE: f64 = 0.02 * SR;
/// The first frame whose delay line is full of the ramp.
const FROM: usize = 1_600;

/// The delay in use at every frame, in frames, by feeding the ramp `x[n] = n` through the
/// runner: a linearly interpolated read of a ramp is exact, so `n - y[n]` is the delay the
/// read used. `retargets` are `(frame, controls)`: the runner is retargeted to them at that
/// frame, wherever it falls in the stream.
fn delay_trace(
    family: &ModDelayFamily,
    first: &ModDelayControls,
    retargets: &[(usize, ModDelayControls)],
    len: usize,
    block: usize,
) -> Vec<f64> {
    let (mut k, _) = built(family, first, RATE);
    let x: Vec<f32> = (0..len).map(|n| n as f32).collect();
    let mut y = Vec::with_capacity(len);
    let mut cuts: Vec<usize> = retargets.iter().map(|r| r.0).collect();
    cuts.push(len);
    let mut at = 0;
    for (cut, next) in cuts.iter().zip(retargets.iter().map(Some).chain([None])) {
        while at < *cut {
            let end = (at + block).min(*cut);
            y.extend(run(&mut k, &[x[at..end].to_vec()], block).remove(0));
            at = end;
        }
        if let Some((_, c)) = next {
            k.retarget(&family.member(c, RATE));
        }
    }
    y.iter()
        .enumerate()
        .map(|(n, &v)| n as f64 - f64::from(v))
        .collect()
}

/// The exact sweep's delay at every frame, in frames: the sine of the LFO's own phase,
/// which accumulates in `f32` (`Phase::advance`) and so runs a little off the ideal
/// `rate * n / sr` over a long render. `rates` are `(frame, hz)`: the rate in force from
/// that frame on.
fn exact_delays(len: usize, depth: f32, rate: f32, rates: &[(usize, f32)]) -> Vec<f64> {
    let mut inc = PhaseIncrement::per_sample(Hz(rate), SampleRate::of(SR));
    let mut phase = Phase::START;
    (0..len)
        .map(|n| {
            if let Some(&(_, hz)) = rates.iter().find(|r| r.0 == n) {
                inc = PhaseIncrement::per_sample(Hz(hz), SampleRate::of(SR));
            }
            let turns = f64::from(phase.get());
            phase = phase.advance(inc);
            BASE + f64::from(depth) * SR * (std::f64::consts::TAU * turns).sin()
        })
        .collect()
}

/// The chord bound of a sine over one control interval, as a delay in frames: `h^2 / 8` of
/// the depth, `h` the radians the interval spans.
fn chord_bound(depth: f32, hz: f32) -> f64 {
    let h = std::f64::consts::TAU * f64::from(hz) * COEFF_INTERVAL as f64 / SR;
    f64::from(depth) * SR * h * h / 8.0
}

/// The delays a sweep reaches are `[max(1, base - depth), base + depth]` frames, an analytic
/// figure of the definition: a 10 ms chorus 5 ms deep at 48 kHz is `240..720`, and a depth
/// past the base **flat-bottoms** at one frame (a delay is at least a frame). It follows
/// the rate, and the depth is clamped into the sweep's own range first.
#[test]
fn a_sweep_reaches_base_plus_and_minus_the_depth_floored_at_a_frame() {
    let chorus = ModDelayFamily::chorus(BusTopology::MONO);
    let m = chorus.member(&controls(1.0, 0.005, 0.0), RATE);
    assert_eq!(m.delay_range(), 240.0..=720.0);
    let m = chorus.member(&controls(1.0, 0.005, 0.0), SampleRate::SR_96K);
    assert_eq!(m.delay_range(), 480.0..=1_440.0);
    // A depth past the base: 1 ms base, 10 ms deep: from one frame to 11 ms.
    let flanger = family(1, short_base(), step(0.0));
    let m = flanger.member(&controls(0.5, 0.01, 0.0), RATE);
    assert_eq!(*m.delay_range().start(), 1.0);
    assert!((f64::from(*m.delay_range().end()) - 0.011 * SR).abs() < 1e-3);
    // The depth is the sweep's: 1 s asks for the greatest (40 ms, 1,920 frames), 10 ms
    // base and all, and a depth of none on the real flanger is its least, 0.1 ms.
    let m = chorus.member(&controls(1.0, 1.0, 0.0), RATE);
    assert_eq!(m.delay_range(), 1.0..=2_400.0);
    assert!(
        (f64::from(m.depth_frames()) - 0.04 * SR).abs() < 1e-2,
        "a depth past the sweep's greatest reads as it: {}",
        m.depth_frames()
    );
    let real = ModDelayFamily::flanger(BusTopology::MONO);
    let m = real.member(&controls(1.0, 0.0, 0.0), RATE);
    assert!((f64::from(m.depth_frames()) - 0.0001 * SR).abs() < 1e-3);
}

/// What is rendered is what the range says: the delay never leaves `[base - depth, base +
/// depth]` and reaches both ends.
#[test]
fn the_rendered_delay_stays_within_the_sweeps_range_and_reaches_it() {
    let (f, depth) = (trajectory(), 0.01f32);
    let c = controls(20.0, f64::from(depth), 0.0);
    let m = f.member(&c, RATE);
    let amp = f64::from(depth) * SR;
    let trace = delay_trace(&f, &c, &[], 12_000, 128);
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for &d in &trace[FROM..] {
        let e = d - BASE;
        assert!(e.abs() <= amp + 0.01, "{e} past +-{amp}");
        lo = lo.min(e);
        hi = hi.max(e);
    }
    assert!(hi > amp * 0.999 && lo < -amp * 0.999, "{lo}..{hi}");
    assert!((f64::from(*m.delay_range().end()) - (BASE + amp)).abs() < 1e-3);
}

/// At a control point (every 16th frame, counted from the start) the delay is the exact
/// sine's; between them the chord misses it by at most `h^2 / 8` of the depth.
#[test]
fn the_delay_is_the_exact_sine_at_control_points_and_within_the_chord_between() {
    let (f, depth, hz) = (trajectory(), 0.01f32, 20.0f32);
    let c = controls(hz, f64::from(depth), 0.0);
    let trace = delay_trace(&f, &c, &[], 12_000, 128);
    let exact = exact_delays(12_000, depth, hz, &[]);
    for n in (FROM..12_000).filter(|n| n % COEFF_INTERVAL == 0) {
        let err = trace[n] - exact[n];
        assert!(err.abs() < 0.05, "frame {n}: off by {err}");
    }
    let bound = chord_bound(depth, hz);
    let worst = (FROM..12_000)
        .map(|n| (trace[n] - exact[n]).abs())
        .fold(0.0, f64::max);
    assert!(worst <= bound + 0.05, "worst {worst} over bound {bound}");
    // At 20 Hz and 10 ms the delay is off the sine by about a tenth of a sample (2 us).
    assert!(
        bound < 0.11,
        "a {COEFF_INTERVAL}-sample spacing is {bound} off"
    );
    // Not vacuous: the chord does miss, by most of the bound.
    assert!(worst > bound * 0.5, "worst {worst}, bound {bound}");
}

/// The sweep keeps its period: successive upward crossings of the centre are `sr / rate`
/// apart.
#[test]
fn the_sweeps_period_is_one_over_the_rate() {
    let (f, depth, hz) = (trajectory(), 0.01f32, 20.0f32);
    let c = controls(hz, f64::from(depth), 0.0);
    let trace = delay_trace(&f, &c, &[], 16_000, 128);
    let mut ups = Vec::new();
    for n in FROM..15_999 {
        let (a, b) = (trace[n] - BASE, trace[n + 1] - BASE);
        if a < 0.0 && b >= 0.0 {
            ups.push(n as f64 + (-a) / (b - a));
        }
    }
    assert!(ups.len() >= 5, "{} crossings", ups.len());
    let period = (ups[ups.len() - 1] - ups[0]) / (ups.len() - 1) as f64;
    let want = SR / f64::from(hz);
    assert!((period - want).abs() < 0.5, "{period} vs {want}");
}

/// A rate that moves **lands at its grid point** and the phase is continuous across it.
/// The rate is whole, but the LFO's sine is solved only at the stream's control points: a
/// retarget at frame 2500, four frames into a segment, leaves the segment it is in on the
/// old rate (its end point was stepped at the old one) and the next begins on the new, so
/// the delay follows the exact sine of a phase that kept running, at the old rate to frame
/// 2512 and at the new one after. Frame 2500 is near a zero crossing, where the delay moves
/// fastest, so a phase that restarted, or a rate that landed a segment early or late, is
/// several frames off.
#[test]
fn a_rate_change_lands_at_its_grid_point_and_the_phase_stays_continuous() {
    let (f, depth) = (trajectory(), 0.01f32);
    let (old, new, len) = (20.0f32, 30.0f32, 4_000);
    let (at, grid) = (2_500, 2_512);
    assert_ne!(at % COEFF_INTERVAL, 0, "mid-segment");
    assert_eq!(grid % COEFF_INTERVAL, 0);
    let first = controls(old, f64::from(depth), 0.0);
    let to = controls(new, f64::from(depth), 0.0);
    let exact = exact_delays(len, depth, old, &[(grid, new)]);
    let bound = chord_bound(depth, new) + 0.05;
    // Cut anywhere in the stream (a block boundary, a segment's middle): the same.
    for block in [1, 16, 100, 128] {
        let trace = delay_trace(&f, &first, &[(at, to)], len, block);
        let worst = (FROM..len)
            .map(|n| (trace[n] - exact[n]).abs())
            .fold(0.0, f64::max);
        assert!(
            worst <= bound,
            "blocks of {block}: worst {worst} over {bound}"
        );
        // Continuous: no frame of the render moves faster than the sweep's own greatest
        // slope at the new rate (`2 pi rate depth`, in frames a frame) and the chord's
        // kink.
        let slope = std::f64::consts::TAU * f64::from(new) * f64::from(depth);
        let jump = (FROM..len - 1)
            .map(|n| (trace[n + 1] - trace[n]).abs())
            .fold(0.0, f64::max);
        assert!(
            jump <= slope * 1.02 + 0.01,
            "blocks of {block}: jump {jump} past {slope}"
        );
    }
    // The old rate holds to the grid point: to frame 2512 the render is the old sine, and
    // the new rate's would be off by far more than the bound.
    let trace = delay_trace(&f, &first, &[(at, to)], len, 128);
    let on_old = exact_delays(len, depth, old, &[]);
    for n in at..grid {
        assert!(
            (trace[n] - on_old[n]).abs() <= chord_bound(depth, old) + 0.05,
            "frame {n} is not on the old rate"
        );
    }
    let sep = (grid..grid + 16)
        .map(|n| (on_old[n] - exact[n]).abs())
        .fold(0.0, f64::max);
    assert!(
        sep > 10.0 * bound,
        "the two rates are told apart: {sep} vs {bound}"
    );
}

/// The rate is **floored at 0.01 Hz**, a lane's value and a cell's alike: a rate below it,
/// zero or negative, is the floor's step, and not a sweep that stands still or runs
/// backwards. From the start the delay rises with the sine at 0.01 Hz: over the first
/// 4,800 frames (0.1 s) it has risen by `depth * sr * sin(2 pi 0.01 * 0.1)`, a figure no
/// other rate gives. A rate that is not a number is the default's.
#[test]
fn a_rate_below_the_floor_is_the_floor() {
    let f = trajectory();
    let floor = PhaseIncrement::per_sample(Hz(0.01), RATE);
    let default = f.member(&ModDelayControls::DEFAULT, RATE).increment();
    for (hz, want) in [
        (0.01f32, floor),
        (0.005, floor),
        (0.0, floor),
        (-3.0, floor),
        (-1e-9, floor),
        (f32::NEG_INFINITY, default),
        (f32::NAN, default),
    ] {
        assert_eq!(
            f.member(&controls(hz, 0.01, 0.0), RATE).increment(),
            want,
            "{hz}"
        );
    }
    let depth = 0.01f32;
    let (len, at) = (6_000, 4_800);
    let c = controls(-3.0, f64::from(depth), 0.0);
    let trace = delay_trace(&f, &c, &[], len, 128);
    let rise = trace[at] - BASE;
    let figure = f64::from(depth) * SR * (std::f64::consts::TAU * 0.01 * 0.1).sin();
    assert!(
        (rise - figure).abs() < 0.05,
        "a negative rate rose {rise}, the 0.01 Hz sweep {figure}"
    );
}

/// A depth that moves is a pitch bend, so it glides at no more than [`DELAY_SLEW`] frames of
/// delay per frame, however far it moves, and **lands exactly**: a depth moved from 5 ms to
/// 40 ms at 48 kHz is 1,680 frames of excursion at `1/8` a frame, 13,440 frames of glide,
/// and the sweep after it is the new depth to the frame. The delay's own motion (a slow
/// 0.1 Hz sweep, at most `2 pi rate depth`) adds to the slew, and nothing else does.
#[test]
fn a_depth_glide_is_bounded_by_the_slew_and_lands_exactly() {
    let f = trajectory();
    let (from, to, hz) = (0.005f64, 0.04f64, 0.1f32);
    // The glide of 13,440 frames straddles the sweep's crest (the quarter period, frame
    // 120,000, where the LFO is at 1), so the excursion's growth is the delay's, a
    // frame a frame at its fastest.
    let (moved, len) = (113_280, 140_000);
    let trace = delay_trace(
        &f,
        &controls(hz, from, 0.0),
        &[(moved, controls(hz, to, 0.0))],
        len,
        64,
    );
    let steady = std::f64::consts::TAU * f64::from(hz) * to;
    let jumps: Vec<f64> = (FROM..len - 1)
        .map(|n| (trace[n + 1] - trace[n]).abs())
        .collect();
    let worst = jumps.iter().copied().fold(0.0, f64::max);
    assert!(
        worst <= f64::from(DELAY_SLEW) + steady * 1.02 + 0.01,
        "the delay moved {worst} frames in one"
    );
    // It does glide, at the slew: at the crest the delay's growth is the excursion's, a
    // frame of delay in 8.
    assert!(worst > f64::from(DELAY_SLEW) * 0.9, "worst {worst}");
    // Landed: after the glide (13,440 frames from the move) the excursion is the new
    // depth's, at the sine of the phase there.
    let landed = moved + 13_440 + 64;
    let amp_at = |n: usize| (trace[n] - BASE) / (to * SR);
    let sine_at = |n: usize| (std::f64::consts::TAU * f64::from(hz) * n as f64 / SR).sin();
    for n in [landed + 1_000, landed + 6_000, len - 100] {
        let want = sine_at(n);
        assert!(
            (amp_at(n) - want).abs() < 2e-3,
            "frame {n}: {} of the new depth, the sine says {want}",
            amp_at(n)
        );
    }
    // And not before: halfway through the glide it is on its way, under the new depth.
    let half = moved + 6_720;
    let on_the_way = (trace[half] - BASE) / SR;
    assert!(
        (on_the_way - (from + (to - from) * 0.5) * sine_at(half)).abs() < 1e-4,
        "{on_the_way} halfway between the depths"
    );
}

/// Channel `c`'s LFO is `c` times the step ahead of channel 0's (a full cycle is no
/// stagger), and the channels' lines are their own: a silent channel stays silent while
/// another sounds, and a ramp through each shows its own delay. The delay of channel `c` at
/// a control point is the exact sine of its phase offset.
#[test]
fn each_channel_is_staggered_by_its_step_and_has_a_line_of_its_own() {
    let sweep = Sweep::CHORUS;
    let c = controls(3.0, 0.005, 0.0);
    // A quarter cycle a channel, and no feedback so a ramp reads off as the delay.
    let f = family(3, sweep, step(0.25));
    let (mut k, _) = built(&f, &c, RATE);
    let len = 8_000;
    let ramp: Vec<f32> = (0..len).map(|n| n as f32).collect();
    let y = run(&mut k, &[ramp.clone(), vec![0.0; len], ramp.clone()], 100);
    assert!(
        y[1].iter().all(|&s| s == 0.0),
        "no bleed into a silent channel"
    );
    // Channels 0 and 2: offsets 0 and a half cycle.
    let inc = PhaseIncrement::per_sample(Hz(3.0), RATE);
    let mut phase = Phase::START;
    let (base, amp) = (0.01 * SR, 0.005 * SR);
    for (n, _) in y[0].iter().enumerate() {
        let turns = f64::from(phase.get());
        phase = phase.advance(inc);
        if n < 600 || n % COEFF_INTERVAL != 0 {
            continue;
        }
        for (ch, offset) in [(0usize, 0.0), (2, 0.5)] {
            let want = base + amp * (std::f64::consts::TAU * (turns + offset)).sin();
            let got = n as f64 - f64::from(y[ch][n]);
            assert!(
                (got - want).abs() < 0.05,
                "channel {ch}, frame {n}: delay {got}, want {want}"
            );
        }
    }
    // A whole cycle of stagger is none: channel 1 of a step of 1.0 sweeps with channel 0.
    let aligned = family(2, sweep, step(1.0));
    let (mut k, _) = built(&aligned, &c, RATE);
    let both = run(&mut k, &[ramp.clone(), ramp.clone()], 100);
    assert_eq!(both[0], both[1], "a full-cycle stagger is no stagger");
    // The offsets can be named: channel 1 half a cycle ahead. The two delays are mirrored
    // about the base.
    let named = ModDelayFamily::new(
        bus(2),
        sweep,
        Stagger::each(&[PhaseIncrement(0.0), PhaseIncrement(0.5)]).unwrap(),
    )
    .unwrap();
    let (mut k, _) = built(&named, &c, RATE);
    let y = run(&mut k, &[ramp.clone(), ramp], 100);
    let mid = 4_000;
    let d = |ch: usize| mid as f64 - f64::from(y[ch][mid]);
    assert!(
        (d(0) + d(1) - 2.0 * base).abs() < 0.2,
        "{} and {} around {base}",
        d(0),
        d(1)
    );
}

/// A re-prepare restarts the sweep: the LFO's phase is zero again, so the delay is the
/// exact sine from the start, as after a `clear`. (The lines are emptied too: the rate is
/// new, and the delays in frames were the old one's.)
#[test]
fn a_re_prepare_restarts_the_sweep() {
    let (f, depth, hz) = (trajectory(), 0.01f32, 20.0f32);
    let c = controls(hz, f64::from(depth), 0.0);
    let (mut k, _) = built(&f, &c, RATE);
    let len = 6_000;
    let x: Vec<f32> = (0..len).map(|n| n as f32).collect();
    // Run a while so the phase is far from zero, mid-segment.
    run(&mut k, &[vec![1.0; 1_003]], 64);
    k.prepare(&f.capacity(RATE), &setup(RATE));
    k.retarget(&f.member(&c, RATE));
    let y = run(&mut k, &[x], 64).remove(0);
    let exact = exact_delays(len, depth, hz, &[]);
    for n in (FROM..len).filter(|n| n % COEFF_INTERVAL == 0) {
        let got = n as f64 - f64::from(y[n]);
        assert!(
            (got - exact[n]).abs() < 0.05,
            "frame {n}: {got} vs {}",
            exact[n]
        );
    }
}

/// A flanger's notches from the definition: the dry and the wet at equal weight, with no
/// feedback and a delay of `D` whole frames, are `1 + z^-D`, `|H| = 2 |cos(w D / 2)|`, with
/// its zeros at `(2k + 1) / (2 D)` of the sample rate and spacing `1 / D`; with feedback
/// `g` the wet's loop is `g z^-D` and the comb **peaks** where `z^-D` is 1, every `1 / D`
/// of the rate. Read off the frozen member's response.
#[test]
fn a_frozen_flanger_has_notches_at_odd_multiples_of_half_over_the_delay() {
    // A base of 1/1024 s at 49,152 Hz is exactly 48 frames, in `f32` too.
    let sweep = Sweep::of(
        Seconds(1.0 / 1024.0),
        MaxDelay::of(Seconds(0.02)),
        Seconds(0.0),
        Seconds(0.01),
        Feedback::MAX_STABLE,
    );
    let f = family(1, sweep, step(0.0));
    let rate = SampleRate::of(49_152.0);
    let m = f.member(&controls(0.5, 0.0, 0.0), rate);
    let d = m.centre_frames().round();
    assert_eq!(d, 48.0);
    let sum_with_dry = |m: &ModDelayMember, freq: f64| -> f64 {
        let mut out = [Complex64::new(0.0, 0.0)];
        m.frequency_response(0, &[Hz(freq as f32)], &mut out);
        (Complex64::new(1.0, 0.0) + out[0]).norm()
    };
    for k in 0..6 {
        let notch = (2.0 * f64::from(k) + 1.0) / (2.0 * f64::from(d)) * rate.get();
        assert!(sum_with_dry(&m, notch) < 1e-3, "notch {k} at {notch} Hz");
        // At every whole multiple of `1 / D` the comb peaks at 2.
        let peak = f64::from(k + 1) / f64::from(d) * rate.get();
        assert!(
            (sum_with_dry(&m, peak) - 2.0).abs() < 1e-3,
            "peak at {peak} Hz"
        );
    }
    // Feedback: the wet's own loop peaks every `1 / D`, at `1 / (1 - g)`.
    let g = 0.7f32;
    let m = f.member(&controls(0.5, 0.0, g), rate);
    let loop_peak = rate.get() / f64::from(d);
    let mut out = [Complex64::new(0.0, 0.0)];
    m.frequency_response(0, &[Hz(loop_peak as f32)], &mut out);
    assert!((out[0].norm() - 1.0 / f64::from(1.0 - g)).abs() < 1e-3);
    assert_eq!(m.loop_frames(), 48, "a loop of `D` frames");
}

/// The loop's period is the delay, as the delay family's and the spec's comb
/// `1 / (1 - g z^-d)`: a frozen sweep (no depth) of a whole `d` frames and a feedback `g`
/// answers an impulse with echoes at frames `d, 2d, 3d, ...` of amplitudes `1, g, g^2, ...`
/// and nothing between. (Before the loop read its feedback tap one frame nearer they landed
/// at `d, 2d + 1, 3d + 2, ...`: a period of `d + 1`.) At a fractional delay `d + f` the read
/// is linear, so one trip is `(1 - f) z^-d + f z^-(d + 1)` and the echoes are its powers:
/// the second, `g` times the square, is `g (1 - f)^2` at `2d`, `2 g f (1 - f)` at `2d + 1`
/// and `g f^2` at `2d + 2`, the first at `d` and `d + 1` of `1 - f` and `f`.
#[test]
fn the_feedback_loops_period_is_the_delay() {
    let rate = SampleRate::of(49_152.0);
    let g = 0.5f32;
    let sweep = |frames: f64| {
        Sweep::of(
            Seconds(frames / rate.get()),
            MaxDelay::of(Seconds(0.02)),
            Seconds(0.0),
            Seconds(0.01),
            Feedback::MAX_STABLE,
        )
    };
    let impulse = |f: &ModDelayFamily| {
        let c = controls(0.5, 0.0, g);
        let (mut k, m) = built(f, &c, rate);
        let mut x = vec![0.0f32; 400];
        x[0] = 1.0;
        (run(&mut k, &[x], 64).remove(0), m)
    };

    // A whole delay of 48 frames: exactly the comb.
    let f = family(1, sweep(48.0), step(0.0));
    let (y, m) = impulse(&f);
    assert_eq!(m.centre_frames(), 48.0);
    assert_eq!(m.loop_frames(), 48);
    for (n, &got) in y.iter().enumerate() {
        let want = if n >= 48 && n % 48 == 0 {
            g.powi((n / 48 - 1) as i32)
        } else {
            0.0
        };
        assert_eq!(got, want, "frame {n}");
    }

    // A fractional delay of 48.5 frames, through the linear read.
    let f = family(1, sweep(48.5), step(0.0));
    let (y, m) = impulse(&f);
    assert!((m.centre_frames() - 48.5).abs() < 1e-4);
    let h = 0.5f32;
    let want = [
        (48, h),
        (49, h),
        (96, g * h * h),
        (97, g * 2.0 * h * h),
        (98, g * h * h),
    ];
    for (n, &got) in y.iter().enumerate().take(99) {
        let w = want.iter().find(|t| t.0 == n).map_or(0.0, |t| t.1);
        assert!((got - w).abs() < 1e-4, "frame {n}: {got} vs {w}");
    }
}

/// `at` freezes the member at any position of the sweep: `base + lfo * depth`, floored at a
/// frame, with no depth left.
#[test]
fn a_member_is_frozen_at_any_position_of_the_sweep() {
    let f = ModDelayFamily::chorus(BusTopology::MONO);
    let m = f.member(&controls(1.0, 0.005, 0.5), RATE);
    for (lfo, want) in [(0.0, 480.0), (1.0, 720.0), (-1.0, 240.0), (0.5, 600.0)] {
        let at = m.at(lfo);
        assert!((at.centre_frames() - want).abs() < 1e-3, "{lfo}");
        assert_eq!(at.depth_frames(), 0.0);
        assert_eq!(at.feedback(), m.feedback());
    }
}

/// A sweep with no feedback at all (the General MIDI chorus) is a pure delay: whatever its
/// feedback control is set to, the member has none, its tail is the sweep's longest delay
/// and a bit (inside its 5 ms line), and its response is a delay (`|H| = 1`).
#[test]
fn a_sweep_with_no_feedback_ignores_the_control_and_is_a_pure_delay() {
    let f = ModDelayFamily::general_midi_chorus(BusTopology::MONO);
    let m = f.member(&controls(0.4, 0.0019, 0.9), RATE);
    assert_eq!(m.feedback(), Feedback::NONE);
    let Ring::Finite(tail) = m.tail(Db(-60.0)) else {
        panic!("a finite tail")
    };
    // The longest the sweep reaches is 3.9 ms; the figure errs long, by the interpolator's
    // frames, and stays inside the line's 5 ms.
    assert!(tail.get() >= 0.0039 && tail.get() <= 0.005, "{tail:?}");
    for freq in [0.0, 1_000.0, 12_345.0, 23_999.0] {
        let mut out = [Complex64::new(0.0, 0.0)];
        m.at(0.0).frequency_response(0, &[Hz(freq)], &mut out);
        assert!(
            (out[0].norm() - 1.0).abs() < 1e-3,
            "{freq} Hz: {}",
            out[0].norm()
        );
    }
}

// ---- the checked constants --------------------------------------------------
//
// `check_range_contract` reads a type through one raw (`new`, `get`): a `Sweep` and a
// `Stagger` are checked on several raws at once, and nothing persists them, so their rules
// are stated here one by one, each with a value on each side of it.

/// A [`Sweep`] refuses what its line could not hold, and never clamps: a base or depth that
/// is not finite or is negative, a least depth above the greatest, a sweep that reaches
/// past the line (`base + depth_max > max_delay`, the transposition `ModDelayConfig` used to
/// clamp silently), and feedback outside `0..=MAX_STABLE`.
#[test]
fn a_sweep_refuses_what_its_line_cannot_hold() {
    let line = MaxDelay::of(Seconds(0.05));
    let ok = |base: f64, lo: f64, hi: f64, fb: f32| {
        Sweep::new(Seconds(base), line, Seconds(lo), Seconds(hi), Feedback(fb))
    };
    // The edge of each rule is in; the next value out is refused.
    assert!(
        ok(0.01, 0.0, 0.04, 0.99).is_ok(),
        "base + depth_max == max_delay"
    );
    assert!(
        ok(0.0, 0.0, 0.05, 0.0).is_ok(),
        "no base, the whole line of depth"
    );
    assert!(
        ok(0.01, 0.04, 0.04, 0.5).is_ok(),
        "a depth range of one value"
    );
    assert_eq!(
        ok(0.011, 0.0, 0.04, 0.5),
        Err(SweepError::Reach {
            base: Seconds(0.011),
            depth_max: Seconds(0.04),
            max_delay: Seconds(0.05)
        }),
        "past the line"
    );
    for bad in [f64::NAN, f64::INFINITY, -0.001] {
        assert!(
            matches!(ok(bad, 0.0, 0.01, 0.5), Err(SweepError::Base(_))),
            "{bad}"
        );
    }
    for (lo, hi) in [
        (0.02, 0.01),
        (-0.001, 0.01),
        (0.0, -0.001),
        (f64::NAN, 0.01),
        (0.0, f64::NAN),
        (0.0, f64::INFINITY),
    ] {
        assert!(
            matches!(ok(0.01, lo, hi, 0.5), Err(SweepError::Depth { .. })),
            "{lo}..{hi}"
        );
    }
    for fb in [-0.01f32, 0.991, 5.0, f32::NAN, f32::INFINITY] {
        assert!(
            matches!(ok(0.01, 0.0, 0.01, fb), Err(SweepError::FeedbackMax(_))),
            "{fb}"
        );
    }
    // The presets are inside their lines, and the accessors say what they were built to.
    for s in [Sweep::CHORUS, Sweep::FLANGER, Sweep::GENERAL_MIDI_CHORUS] {
        assert!(s.base().get() + s.depth_max().get() <= s.max_delay().get().get());
    }
    assert_eq!(Sweep::GENERAL_MIDI_CHORUS.feedback_max(), Feedback::NONE);
    assert_eq!(Sweep::CHORUS.depth_max(), Seconds(0.04));
}

/// `of` panics where `new` refuses, the constant's spelling.
#[test]
#[should_panic(expected = "a sweep stays inside its line")]
fn a_sweep_of_a_transposed_base_panics_where_it_is_a_constant() {
    let _ = Sweep::of(
        Seconds(0.05),
        MaxDelay::of(Seconds(0.01)),
        Seconds(0.0),
        Seconds(0.001),
        Feedback::NONE,
    );
}

/// A [`Stagger`] is finite, and wrapped into one cycle where it is made: a step of `1.25`
/// is a quarter cycle, an offset of `-0.25` is three quarters, and channel `c` of a step
/// is `c` times it, wrapped. One offset for each channel is checked against the bus by the
/// family that takes it.
#[test]
fn a_stagger_is_finite_and_wrapped_into_one_cycle() {
    assert!(Stagger::step(PhaseIncrement(0.25)).is_some());
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(Stagger::step(PhaseIncrement(bad)), None, "{bad}");
        assert!(matches!(
            Stagger::each(&[PhaseIncrement(0.0), PhaseIncrement(bad)]),
            Err(StaggerError::NotFinite { channel: 1, .. })
        ));
    }
    let s = Stagger::step(PhaseIncrement(1.25)).unwrap();
    assert_eq!(s.offset(0), PhaseIncrement(0.0));
    assert_eq!(s.offset(1), PhaseIncrement(0.25));
    assert_eq!(s.offset(2), PhaseIncrement(0.5));
    assert_eq!(s.offset(4), PhaseIncrement(0.0), "a full cycle is none");
    let back = Stagger::step(PhaseIncrement(-0.25)).unwrap();
    assert_eq!(
        back.offset(1),
        PhaseIncrement(0.75),
        "behind is wrapped ahead"
    );
    let each = Stagger::each(&[PhaseIncrement(0.0), PhaseIncrement(-0.25)]).unwrap();
    assert_eq!(each.offset(1), PhaseIncrement(0.75));
    assert_eq!(each.offset(7), PhaseIncrement(0.0), "past the named: none");
    assert_eq!((each.channels(), s.channels()), (Some(2), None));
    let too_many = [PhaseIncrement(0.0); MAX_BUS_CHANNELS + 1];
    assert_eq!(
        Stagger::each(&too_many),
        Err(StaggerError::TooMany {
            count: MAX_BUS_CHANNELS + 1,
            max: MAX_BUS_CHANNELS
        })
    );
    assert!(Stagger::each(&too_many[..MAX_BUS_CHANNELS]).is_ok());
}

/// A family refuses offsets that are not one for each channel of its bus, and takes a step
/// at any width.
#[test]
fn a_family_refuses_offsets_that_do_not_fit_its_bus() {
    let two = Stagger::each(&[PhaseIncrement(0.0), PhaseIncrement(0.5)]).unwrap();
    assert!(ModDelayFamily::new(BusTopology::STEREO, Sweep::CHORUS, two).is_ok());
    assert_eq!(
        ModDelayFamily::new(bus(3), Sweep::CHORUS, two).unwrap_err(),
        StaggerError::Length { width: 3, got: 2 }
    );
    assert_eq!(
        ModDelayFamily::new(BusTopology::MONO, Sweep::CHORUS, two).unwrap_err(),
        StaggerError::Length { width: 1, got: 2 }
    );
    for width in [1, 2, 6, 16] {
        assert!(ModDelayFamily::new(bus(width), Sweep::CHORUS, step(0.25)).is_ok());
    }
}
