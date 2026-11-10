//! The realization contract (design 022 section 8.2): cases every [`Realization`] must
//! pass against the [`Network`] of its [`Design`], measured on its output.
//!
//! The design contract ([`super::design`]) holds a network to the analytic
//! figures its controls ask for; this one holds the realization to the network. A
//! realization implements [`RealizationCase`] (its tolerance, the controls to probe and
//! the bound on its output) and runs [`realization_contract!`], which makes one
//! `#[test]` per case. Every figure is the network's own query (`rt60`, `tail`,
//! `min_delay`, `latency`, `has_direct_path`) or a property of the render
//! (finite, bounded, silent before the first arrival, bit-identical after
//! `reset`), never another implementation's output.
//!
//! **Each case runs against a deliberately broken realization it must fail**: a
//! [`Defective`] design hands out a [`Flawed`] realization, a real realization with one
//! [`Defect`] added around it, and the test beside the suite asserts the case
//! reports the failure (a case that cannot fail proves nothing).
//!
//! What the suite leaves to the design it runs over: a realization with no freeze,
//! pre-delay, width or wet control has nothing to say in those directions.
//! Freeverb's network never sets a pre-delay, a width or a wet gain, so those
//! directions are vacuous for it (the arrival case still places its pre-delay,
//! zero, and the lossless case is the design contract's). Allocation-freedom of
//! `retarget` and `process` needs a counting allocator, which `unsafe` forbids
//! in this crate's lib tests: `tests/network_alloc.rs` runs it.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{Db, MaxBlock, SampleRate, Samples};

use super::design::{DesignCase, map_absorption, map_fdn};
use crate::network::{Absorption, Band, Bounds, Network};
use crate::{Ceiling, Channels, Design, Effect, MaxDecay, Realization, Ring, Setup, System};

/// The longest block the cases prepare for.
const MAX_BLOCK: usize = 512;
/// The window the energy of a decay is measured in, in frames.
const WINDOW: usize = 1024;

/// A design plus what the realization contract needs to know about its realization.
pub(crate) trait RealizationCase: DesignCase {
    /// The relative error the realization's measured RT60 may have against the
    /// network's `rt60` (the realization declares it: design 022 section 8.2).
    fn tolerance(&self) -> f32;

    /// The controls the measured cases (RT60, tail, bands) run over: a few,
    /// spread across the controls' range, because each renders seconds of audio.
    fn probes(&self) -> Vec<Self::Controls>;

    /// The controls at the extreme: the longest decay and the largest size,
    /// where a loop is closest to unstable.
    fn extremes(&self) -> Vec<Self::Controls>;

    /// `(a, b)`: two controls a step apart, for the click-free case.
    fn steps(&self) -> Vec<(Self::Controls, Self::Controls)>;

    /// The largest `|output|` the network can give for an input in `-1..=1`,
    /// from its analysis (an L1 norm of its impulse response).
    fn peak_bound(&self, controls: &Self::Controls, network: &Network) -> f32;

    /// The depths, in dB below the plateau, between which the decay is fitted.
    /// 40 to 65 by default: Freeverb flushes values under 1e-6 to zero, which
    /// a deeper fit would measure.
    fn fit_depth(&self) -> (f64, f64) {
        (40.0, 65.0)
    }

    /// The frequency, in cycles a frame, the burst of `band` is played at: the
    /// three points [`Band`] names for a network that is not given by bands (DC,
    /// a quarter of the rate, Nyquist) by default.
    fn tone(&self, band: Band, rate: SampleRate) -> f32 {
        let _ = rate;
        [0.0, 0.25, 0.5][band.index()]
    }

    /// The controls whose first echoes the realization must place to the frame:
    /// `probes` without the modulation of the line lengths, which moves an echo
    /// off the network's centre figure. All of `probes` by default.
    fn static_probes(&self) -> Vec<Self::Controls> {
        self.probes()
    }

    /// `(thawed, frozen)`: the same controls without and with the freeze.
    fn freeze_pairs(&self) -> Vec<(Self::Controls, Self::Controls)> {
        Vec::new()
    }

    /// `(natural, mono)`: the same controls at the stereo width the design calls
    /// natural and at none.
    fn width_pairs(&self) -> Vec<(Self::Controls, Self::Controls)> {
        Vec::new()
    }

    /// `(a, b, ratio)`: `b` is the same sound as `a` at `ratio` times the level
    /// (`0` for an exact silence).
    fn wet_pairs(&self) -> Vec<(Self::Controls, Self::Controls, f32)> {
        Vec::new()
    }
}

// ---- the harness -------------------------------------------------------------------

fn setup<D: Design>(d: &D, rate: SampleRate) -> Setup {
    let Channels { input, output } = d.channels();
    Setup {
        rate,
        max_block: MaxBlock::of(Samples(MAX_BLOCK)),
        input,
        output,
    }
}

/// A prepared realization retargeted to `controls`, and its network.
fn built<D: Design<Model = Network>>(
    d: &D,
    c: &D::Controls,
    rate: SampleRate,
) -> (D::Realization, Network) {
    let ceiling = Ceiling::at(rate).expect("a positive finite rate");
    let mut k = d.realization();
    k.prepare(&d.bounds(&ceiling), &setup(d, rate));
    let network = d.design(c, rate);
    k.retarget(&network);
    (k, network)
}

/// The input and output widths.
fn widths<D: Design>(d: &D) -> (usize, usize) {
    let Channels { input, output } = d.channels();
    (usize::from(input.count()), usize::from(output.count()))
}

/// Renders `input` (the same signal on every input channel) through `k` in
/// blocks of `block`, one `Vec` per output channel.
fn render<K: Effect<Network>>(
    k: &mut K,
    (inputs, outputs): (usize, usize),
    input: &[f32],
    block: usize,
) -> Vec<Vec<f32>> {
    let mut out = vec![vec![0.0f32; input.len()]; outputs];
    let mut start = 0;
    while start < input.len() {
        let end = (start + block).min(input.len());
        let ins: Vec<&[f32]> = vec![&input[start..end]; inputs];
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[start..end]).collect();
        k.process(&ins, &mut outs);
        start = end;
    }
    out
}

fn noise(seed: u64, frames: usize, level: f32) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(seed);
    (0..frames).map(|_| rng.gen_range(-level..=level)).collect()
}

/// A tone burst at `freq` cycles a frame for `plateau` frames between
/// raised-cosine fades of [`FADE`] frames, then `after` frames of silence. The
/// fades keep the burst to its band: an abrupt start or stop leaks energy into
/// the low frequencies, which decay slowest and would govern the deep part of a
/// high band's decay.
struct Burst {
    signal: Vec<f32>,
    /// Where the plateau ends and the fade-out begins.
    plateau_end: usize,
    /// Where the input is silent for good.
    end: usize,
}

/// The length of a burst's fades, in frames.
const FADE: usize = 4096;

fn burst(freq: f32, plateau: usize, after: usize) -> Burst {
    let end = plateau + 2 * FADE;
    let mut signal = vec![0.0f32; end + after];
    for (n, s) in signal[..end].iter_mut().enumerate() {
        let tone = libm::cos(std::f64::consts::TAU * f64::from(freq) * n as f64) as f32;
        let fade = |k: usize| {
            let x = std::f32::consts::PI * k as f32 / FADE as f32;
            0.5 - 0.5 * x.cos()
        };
        let envelope = if n < FADE {
            fade(n)
        } else if n >= FADE + plateau {
            fade(end - n)
        } else {
            1.0
        };
        *s = tone * envelope;
    }
    Burst {
        signal,
        plateau_end: FADE + plateau,
        end,
    }
}

fn energy(out: &[Vec<f32>], from: usize, len: usize) -> f64 {
    out.iter()
        .flat_map(|c| &c[from..from + len])
        .map(|&s| f64::from(s) * f64::from(s))
        .sum()
}

/// How many frames of burst plateau make `lines` trips of the longest line.
fn burst_frames(bounds: &Bounds, lines: f32) -> usize {
    (lines * bounds.line) as usize
}

/// The RMS of `out` over `len` frames from `from`, all channels together.
fn rms(out: &[Vec<f32>], from: usize, len: usize) -> f64 {
    (energy(out, from, len) / (len * out.len()) as f64).sqrt()
}

/// The RT60 of the decay after the burst: a least squares line through the
/// energy of the windows between 40 and 65 dB below the plateau's, `60 / -slope`.
/// Deep, because a bank of lines of different length decays at the slowest
/// line's rate in the end, and not deeper, because a realization flushes values
/// under 1e-6 (Freeverb's does, which the network does not model) and a deeper
/// fit measures the flush.
fn measured_rt60(
    out: &[Vec<f32>],
    b: &Burst,
    rate: SampleRate,
    bounds: &Bounds,
    (from, to): (f64, f64),
) -> Result<f32, String> {
    // Windows twice the longest line, stepping by `WINDOW`: a train of echoes
    // (parallel combs) puts energy in spikes a line apart, and a window shorter
    // than that measures where the spikes fall.
    let long = ((2.0 * bounds.line) as usize).max(4 * WINDOW);
    let reference = energy(out, b.plateau_end - long, long);
    if reference <= 0.0 {
        return Err("nothing sounds before the input stops".into());
    }
    let len = out[0].len();
    let (mut n, mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    let mut start = b.end;
    while start + long <= len {
        let db = 10.0 * (energy(out, start, long) / reference).log10();
        if (-to..=-from).contains(&db) {
            let t = (start - b.end + long / 2) as f64 / rate.get();
            n += 1.0;
            sx += t;
            sy += db;
            sxx += t * t;
            sxy += t * db;
        }
        start += WINDOW;
    }
    if n < 4.0 {
        let first = 10.0 * (energy(out, b.end, long) / reference).log10();
        let last = 10.0 * (energy(out, len - long, long) / reference).log10();
        return Err(format!(
            "only {n} windows between -{from} and -{to} dB (first window {first:.1} dB, last {last:.1} dB)"
        ));
    }
    let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
    if slope >= 0.0 {
        return Err(format!("the energy does not fall (slope {slope} dB/s)"));
    }
    Ok((60.0 / -slope) as f32)
}

fn rate_of<D: DesignCase>(d: &D) -> SampleRate {
    d.rates()[0]
}

/// Frames of silence after a burst that reach 65 dB down at the slowest rate
/// the network gives in any band, with room (a realization whose bands decay slower
/// than its network says is measured, not cut short).
fn rt60_frames(network: &Network, rate: SampleRate, depth: f64) -> Result<usize, String> {
    let r = Band::ALL
        .iter()
        .map(|&b| network.rt60(b).get())
        .fold(0.0, f64::max);
    if r.is_finite() && r > 0.0 {
        Ok((r * rate.get() * (depth / 60.0 + 0.35)) as usize + 8 * WINDOW)
    } else {
        Err(format!("a network with no finite RT60 ({r})"))
    }
}

// ---- the cases ---------------------------------------------------------------------

pub(crate) mod cases {
    use super::*;

    /// A tone burst in each band, stopped; the energy decay after it has the RT60
    /// the network's `rt60(band)` says, within the realization's tolerance (measured
    /// from 40 to 65 dB down, where the slowest line governs).
    ///
    /// Mutation (a `Defective` test): the realization retargets every loop gain
    /// scaled by 1.05, 39% longer to fall 60 dB.
    pub(crate) fn rt60<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let tol = d.tolerance();
        let mut measured = 0;
        for c in d.static_probes() {
            for band in Band::ALL {
                let (mut k, network) = built(d, &c, rate);
                let ceiling = Ceiling::at(rate).expect("a rate");
                let frames = burst_frames(&d.bounds(&ceiling), 3.0);
                let b = burst(
                    d.tone(band, rate),
                    frames,
                    rt60_frames(&network, rate, d.fit_depth().1)?,
                );
                let out = render(&mut k, widths(d), &b.signal, 64);
                let got = measured_rt60(&out, &b, rate, &d.bounds(&ceiling), d.fit_depth())?;
                let want = network.rt60(band).to_f32();
                measured += 1;
                if (got - want).abs() > tol * want {
                    return Err(format!(
                        "{band:?}: measured RT60 {got:.3} s, network says {want:.3} s \
                         (tolerance {tol})"
                    ));
                }
            }
        }
        if measured == 0 {
            return Err("no controls probed".into());
        }
        Ok(())
    }

    /// The measured bands fall in the order the network gives them: for every pair
    /// of bands whose network RT60s differ by more than twice the tolerance, the
    /// measured ones are ordered alike (and differ by more than the tolerance).
    ///
    /// Mutation: the realization ignores the damping (every band decays like the
    /// lowest).
    pub(crate) fn band_order<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let tol = d.tolerance();
        let mut ordered = 0;
        for c in d.static_probes() {
            let ceiling = Ceiling::at(rate).expect("a rate");
            let frames = burst_frames(&d.bounds(&ceiling), 3.0);
            let mut got = [0.0f32; 3];
            let mut want = [0.0f32; 3];
            for band in Band::ALL {
                let (mut k, network) = built(d, &c, rate);
                let b = burst(
                    d.tone(band, rate),
                    frames,
                    rt60_frames(&network, rate, d.fit_depth().1)?,
                );
                let out = render(&mut k, widths(d), &b.signal, 64);
                got[band.index()] =
                    measured_rt60(&out, &b, rate, &d.bounds(&ceiling), d.fit_depth())?;
                want[band.index()] = network.rt60(band).to_f32();
            }
            for (i, j) in [(0, 1), (1, 2), (0, 2)] {
                if (want[i] - want[j]).abs() > 2.0 * tol * want[i].max(want[j]) {
                    ordered += 1;
                    if (want[i] > want[j]) != (got[i] > got[j])
                        || (got[i] - got[j]).abs() <= tol * got[i].max(got[j])
                    {
                        return Err(format!(
                            "bands {i},{j}: network {want:?}, measured {got:?} (tolerance {tol})"
                        ));
                    }
                }
            }
        }
        if ordered == 0 {
            return Err("no control separates the bands".into());
        }
        Ok(())
    }

    /// The time from the input's end to each floor (-40, -60 and -90 dB below the
    /// loudest windowed RMS level it reached; a short burst has not
    /// yet filled the lines, so the level after the input stops can exceed the
    /// level at the stop) is within the network's `tail(floor)`: what the node
    /// reports to the graph covers what the realization rings.
    ///
    /// Mutation: the realization retargets every loop gain scaled by 1.03, which rings
    /// 2.4 times as long at the largest room and so past the network's tail.
    pub(crate) fn tail_covers<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let mut checked = 0;
        for c in d.probes() {
            let (mut k, network) = built(d, &c, rate);
            let ceiling = Ceiling::at(rate).expect("a rate");
            let frames = burst_frames(&d.bounds(&ceiling), 12.0);
            let Ring::Finite(deepest) = network.tail(Db(-90.0)) else {
                continue;
            };
            let after = (deepest.get() * rate.get()) as usize + 4 * WINDOW;
            let b = burst(d.tone(Band::Low, rate), frames, after);
            let out = render(&mut k, widths(d), &b.signal, 64);
            let carried = (0..=out[0].len() - WINDOW)
                .step_by(WINDOW / 4)
                .map(|n| rms(&out, n, WINDOW))
                .fold(0.0, f64::max);
            if carried <= 0.0 {
                return Err("nothing sounds before the input stops".into());
            }
            for floor in [-40.0f32, -60.0, -90.0] {
                let Ring::Finite(tail) = network.tail(Db(floor)) else {
                    continue;
                };
                let limit = carried * f64::from(10f32.powf(floor / 20.0));
                // The last window still at or above the floor, at its middle.
                let last = (b.end..=out[0].len() - WINDOW)
                    .rev()
                    .step_by(WINDOW / 4)
                    .find(|&n| rms(&out, n, WINDOW) >= limit);
                let took = last.map_or(0, |n| n + WINDOW / 2 - b.end);
                let allowed = (tail.get() * rate.get()).ceil() as usize;
                checked += 1;
                if last.is_some_and(|n| n + 2 * WINDOW >= out[0].len()) {
                    return Err(format!(
                        "still sounding at {floor} dB at the end of the render"
                    ));
                }
                if took > allowed {
                    return Err(format!(
                        "rings {took} frames to {floor} dB, the network's tail is {allowed}"
                    ));
                }
            }
        }
        if checked == 0 {
            return Err("every probe has an unbounded tail".into());
        }
        Ok(())
    }

    /// A unit impulse arrives first exactly `min_delay + latency`
    /// frames in (the network's own shortest path, pre-delay included, and the
    /// realization's declared latency),
    /// on the earliest channel, and on no channel before.
    ///
    /// Mutations: the realization's output delayed by one frame (its `latency` still
    /// says the same); a `latency` that claims one frame more than the realization
    /// delays.
    pub(crate) fn arrival<D: RealizationCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in d.static_probes() {
                let (mut k, network) = built(d, &c, rate);
                let expected = network.min_delay() as usize + k.latency().get();
                let mut x = vec![0.0f32; expected + 4000];
                x[0] = 1.0;
                let out = render(&mut k, widths(d), &x, 64);
                let first = out
                    .iter()
                    .filter_map(|c| c.iter().position(|&s| s != 0.0))
                    .min();
                if first != Some(expected) {
                    return Err(format!(
                        "first output at {first:?}, network and latency say {expected} at {rate:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// The realization declares the network's own latency (zero for a time-domain
    /// network, a partition for a FIR), never a pre-delay or a tap spacing.
    ///
    /// Mutation: a realization that declares one frame more than it has.
    pub(crate) fn latency<D: RealizationCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in d.static_probes() {
                let (k, network) = built(d, &c, rate);
                if k.latency() != network.latency() {
                    return Err(format!(
                        "declares {} frames of latency, the network has {}",
                        k.latency(),
                        network.latency()
                    ));
                }
            }
        }
        Ok(())
    }

    /// The realization is wet only: with noise in from the first frame, every output
    /// channel is exactly zero until the network's shortest path has been
    /// travelled, so none of the input reaches the output.
    ///
    /// Mutation: the realization adds its input to its first output.
    pub(crate) fn wet_only<D: RealizationCase>(d: &D) -> Result<(), String> {
        if !d.wet_only() {
            return Ok(());
        }
        for rate in d.rates() {
            for c in d.static_probes() {
                let (mut k, network) = built(d, &c, rate);
                let silent = network.min_delay() as usize + k.latency().get();
                let x = noise(7, silent + 512, 1.0);
                let out = render(&mut k, widths(d), &x, 64);
                if let Some(n) = out
                    .iter()
                    .flat_map(|c| &c[..silent])
                    .position(|&s| s != 0.0)
                {
                    return Err(format!(
                        "sound before the shortest path ({silent} frames): item {n}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// A stereo output is decorrelated: the zero-lag correlation of the two
    /// channels' response to noise stays under 0.3. A design with one output
    /// channel has nothing to say.
    ///
    /// Mutation: the realization copies its first output channel onto its second.
    pub(crate) fn decorrelated<D: RealizationCase>(d: &D) -> Result<(), String> {
        let (_, outputs) = widths(d);
        if outputs < 2 {
            return Ok(());
        }
        let rate = rate_of(d);
        for c in d.probes() {
            let (mut k, _) = built(d, &c, rate);
            let out = render(&mut k, widths(d), &noise(11, 1 << 16, 1.0), 64);
            let dot = |a: &[f32], b: &[f32]| -> f64 {
                a.iter()
                    .zip(b)
                    .map(|(&x, &y)| f64::from(x) * f64::from(y))
                    .sum()
            };
            let corr = dot(&out[0], &out[1])
                / (dot(&out[0], &out[0]) * dot(&out[1], &out[1]))
                    .sqrt()
                    .max(f64::MIN_POSITIVE);
            if corr.abs() >= 0.3 {
                return Err(format!("the channels correlate at {corr:.3}"));
            }
        }
        Ok(())
    }

    /// Sixty seconds of noise at the extreme controls stays finite and under the
    /// bound the network's analysis gives, when the network is stable.
    ///
    /// Mutation: a gain that grows by 0.01% a frame, standing in for a loop gain
    /// above unity (a `LoopGain` mutant would not do: Freeverb's `retarget`
    /// refuses a loop gain of 1 or more, so scaling the network's cannot
    /// destabilise it).
    pub(crate) fn stability<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let frames = (60.0 * rate.get()) as usize;
        let mut checked = 0;
        for c in d.extremes() {
            let (mut k, network) = built(d, &c, rate);
            if !network.is_stable() {
                continue;
            }
            let bound = d.peak_bound(&c, &network);
            let out = render(&mut k, widths(d), &noise(3, frames, 1.0), 512);
            let worst = out.iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()));
            checked += 1;
            if out.iter().flatten().any(|s| !s.is_finite()) {
                return Err("a non-finite sample".into());
            }
            if worst > bound {
                return Err(format!("peak {worst} against a bound of {bound}"));
            }
        }
        if checked == 0 {
            return Err("no stable extreme to run".into());
        }
        Ok(())
    }

    /// A retarget mid-tail does not click: a 150 Hz tone through the realization, the
    /// controls stepped from `a` to `b` at a block edge, and the largest
    /// sample-to-sample jump after the step is within 1.5 times the larger of the
    /// two steady states' (each measured on a realization left at its controls). A
    /// tone, not noise: its steady jumps are small, so a step in a gain or a
    /// jump in a delay is a jump many times them, where noise hides it.
    ///
    /// Mutations: the realization scales its output by 4 from its second retarget on,
    /// a gain applied unslewed (a `Defective` test); an FDN that takes its new
    /// gains, or its new line lengths, whole (run by hand on `fdn.rs`).
    pub(crate) fn click_free<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let second = rate.get() as usize;
        let tone: Vec<f32> = (0..6 * second)
            .map(|n| 0.5 * libm::sin(std::f64::consts::TAU * 150.0 * n as f64 / rate.get()) as f32)
            .collect();
        let jump = |out: &[Vec<f32>], from: usize, to: usize| -> f32 {
            out.iter()
                .flat_map(|c| c[from..to].windows(2))
                .fold(0.0, |m, w| m.max((w[1] - w[0]).abs()))
        };
        let steady = |c: &D::Controls| -> f32 {
            let (mut k, _) = built(d, c, rate);
            let out = render(&mut k, widths(d), &tone, 64);
            jump(&out, 4 * second, 6 * second)
        };
        let mut checked = 0;
        for (a, b) in d.steps() {
            let (mut k, _) = built(d, &a, rate);
            let shape = widths(d);
            let mut out = render(&mut k, shape, &tone[..3 * second], 64);
            k.retarget(&d.design(&b, rate));
            let after = render(&mut k, shape, &tone[3 * second..], 64);
            for (all, more) in out.iter_mut().zip(after) {
                all.extend(more);
            }
            let click = jump(&out, 3 * second - 1, 6 * second);
            let allowed = 3.0 * steady(&a).max(steady(&b));
            checked += 1;
            if click > allowed {
                return Err(format!(
                    "a jump of {click} after the step, steady states allow {allowed}"
                ));
            }
        }
        if checked == 0 {
            return Err("no step to try".into());
        }
        Ok(())
    }

    /// Every line of a first recirculating stage that feeds the output through
    /// the stages after it, with nothing but series all-passes after: with the
    /// loop gain high, an impulse's first echo of line `i` lands at
    /// `pre_delay + length_i` and is the input gain's size (a Freeverb all-pass
    /// chain of four passes the first sample at full size). Pins every line
    /// length, not only the shortest that `arrival` finds. Vacuous (an error) for
    /// a network of another shape.
    ///
    /// Mutation: the last right comb (1640 frames, the longest) 5% long, which is
    /// inside the RT60 tolerance.
    pub(crate) fn line_lengths<D: RealizationCase>(d: &D) -> Result<(), String> {
        use crate::network::Stage;
        let rate = rate_of(d);
        let mut checked = 0;
        for c in d.static_probes() {
            let (mut k, network) = built(d, &c, rate);
            let Some(Stage::Fdn(f)) = network.stages.first() else {
                continue;
            };
            if !network.stages[1..]
                .iter()
                .all(|s| matches!(s, Stage::AllPass(_)))
            {
                continue;
            }
            let mut x = vec![0.0f32; 4096];
            x[0] = 1.0;
            let out = render(&mut k, widths(d), &x, 64);
            // An all-pass echoes an earlier line's first echo `delay` frames
            // later, which can cancel this line's (Freeverb's 1116 + 441 = 1557):
            // such a line is read at its other end, by the lines that do not.
            let delays: Vec<usize> = network.stages[1..]
                .iter()
                .flat_map(|s| match s {
                    Stage::AllPass(a) => a.iter().map(|a| a.delay as usize).collect(),
                    _ => Vec::new(),
                })
                .collect();
            for (i, line) in f.lines.iter().enumerate() {
                let masked = f.lines.iter().enumerate().any(|(j, other)| {
                    j != i
                        && delays
                            .iter()
                            .any(|&d| other.length as usize + d == line.length as usize)
                });
                if masked {
                    continue;
                }
                let at = network.pre_delay.get() + line.length as usize;
                // The rows that take this line.
                for (r, row) in f.output.rows.iter().enumerate() {
                    if row[i] == 0.0 {
                        continue;
                    }
                    checked += 1;
                    let want = 0.5 * (f.input[i] * row[i]).abs();
                    if out[r][at].abs() < want {
                        return Err(format!(
                            "line {i} (length {}, pre {} at {at}, row {r}): output {} at its first echo, wanted {want}",
                            line.length,
                            network.pre_delay.get(),
                            out[r][at]
                        ));
                    }
                }
            }
        }
        if checked == 0 {
            return Err("no first-echo line to check".into());
        }
        Ok(())
    }

    /// After `reset` the realization renders bit for bit what a freshly built one
    /// does, at the same controls.
    ///
    /// Mutation: a `reset` that does nothing.
    pub(crate) fn reset<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        for c in d.probes() {
            let probe = noise(9, 8192, 1.0);
            let (mut used, _) = built(d, &c, rate);
            render(&mut used, widths(d), &noise(8, 20_000, 1.0), 64);
            used.reset();
            let (mut fresh, _) = built(d, &c, rate);
            if render(&mut used, widths(d), &probe, 64) != render(&mut fresh, widths(d), &probe, 64)
            {
                return Err("a reset realization renders differently from a fresh one".into());
            }
        }
        Ok(())
    }

    /// `fresh()` of a used realization renders bit for bit what a freshly built one
    /// does, at the same controls: state gone, coefficients kept.
    ///
    /// Mutation: a `fresh` that keeps the state (a plain clone).
    pub(crate) fn fresh<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        for c in d.probes() {
            let probe = noise(9, 8192, 1.0);
            let (mut used, _) = built(d, &c, rate);
            render(&mut used, widths(d), &noise(8, 20_000, 1.0), 64);
            let mut forked = used.fresh();
            let (mut fresh, _) = built(d, &c, rate);
            if render(&mut forked, widths(d), &probe, 64)
                != render(&mut fresh, widths(d), &probe, 64)
            {
                return Err("fresh() renders differently from a freshly built realization".into());
            }
        }
        Ok(())
    }

    /// The output does not depend on how the input is cut into blocks: blocks of
    /// 1, 17 and 64 frames render bit for bit what blocks of 512 do (a
    /// time-domain realization; a convolver would declare its own tolerance).
    ///
    /// Mutation: a gain that depends on the block's length.
    pub(crate) fn block_independent<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        for c in d.probes() {
            let x = noise(13, 4096, 1.0);
            let (mut k, _) = built(d, &c, rate);
            let whole = render(&mut k, widths(d), &x, MAX_BLOCK);
            for block in [1, 17, 64] {
                let (mut k, _) = built(d, &c, rate);
                if render(&mut k, widths(d), &x, block) != whole {
                    return Err(format!(
                        "blocks of {block} differ from blocks of {MAX_BLOCK}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// The first frame any channel sounds for a unit impulse at frame 0 of the
    /// network `c` designs, in `frames`.
    fn first_sound<D: RealizationCase>(d: &D, c: &D::Controls, rate: SampleRate) -> Option<usize> {
        let (mut k, network) = built(d, c, rate);
        let frames = network.min_delay() as usize + 4000;
        let mut x = vec![0.0f32; frames];
        x[0] = 1.0;
        render(&mut k, widths(d), &x, 64)
            .iter()
            .filter_map(|ch| ch.iter().position(|&s| s != 0.0))
            .min()
    }

    /// A pre-delay moves the first sound by exactly its frames, and `latency()`
    /// stays what it was: pre-delay is part of the wet sound, never latency.
    /// Vacuous for a design with no pre-delay control.
    ///
    /// Mutation: the realization drops the pre-delay (a `Defective` test).
    pub(crate) fn pre_delay<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        for (without, with, by) in d.pre_delay_pairs() {
            let (a, b) = (first_sound(d, &without, rate), first_sound(d, &with, rate));
            let want = by.to_samples(rate).get();
            if a.zip(b).is_none_or(|(a, b)| b != a + want) {
                return Err(format!(
                    "a pre-delay of {want} frames moved the first sound from {a:?} to {b:?}"
                ));
            }
            if built(d, &without, rate).0.latency() != built(d, &with, rate).0.latency() {
                return Err("a pre-delay changed the latency".into());
            }
        }
        Ok(())
    }

    /// A larger room sounds later: the first sound of `b` is after `a`'s.
    /// Vacuous for a design with no size control.
    ///
    /// Mutation: the realization keeps every line at one length.
    pub(crate) fn size<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        for (a, b) in d.size_pairs() {
            let (fa, fb) = (first_sound(d, &a, rate), first_sound(d, &b, rate));
            if fa.zip(fb).is_none_or(|(x, y)| y <= x) {
                return Err(format!("a bigger room sounded at {fb:?}, not after {fa:?}"));
            }
        }
        Ok(())
    }

    /// The fraction of frames in `[from, to)` where some channel is above a
    /// quarter of the window's RMS: near 1 for a dense tail, small for a train
    /// of echoes.
    fn fullness(out: &[Vec<f32>], from: usize, to: usize) -> f64 {
        let rms = rms(out, from, to - from);
        let above = (from..to)
            .filter(|&n| out.iter().any(|c| f64::from(c[n].abs()) > 0.25 * rms))
            .count();
        above as f64 / (to - from) as f64
    }

    /// More diffusion measures denser: in the window from 0.15 to 0.3 s of an
    /// impulse's response the denser controls fill at least 0.15 more of the
    /// frames. Vacuous for a design with no diffusion control.
    ///
    /// Mutation: the realization mixes no line into another.
    pub(crate) fn diffusion<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let (from, to) = ((0.15 * rate.get()) as usize, (0.3 * rate.get()) as usize);
        let mut x = vec![0.0f32; to + 1];
        x[0] = 1.0;
        for (sparse, dense) in d.diffusion_pairs() {
            let measure = |c: &D::Controls| {
                let (mut k, _) = built(d, c, rate);
                fullness(&render(&mut k, widths(d), &x, 64), from, to)
            };
            let (a, b) = (measure(&sparse), measure(&dense));
            if b < a + 0.15 {
                return Err(format!(
                    "more diffusion filled {b:.3} of the frames, not over {a:.3}"
                ));
            }
        }
        Ok(())
    }

    /// A frozen tail neither grows nor loses, and takes no input: noise to fill
    /// the lines, then the freeze; a second later the level is `L`, nine seconds
    /// later it is within 0.25 dB of it, and a second of loud noise on top
    /// leaves it there. Vacuous for a design that cannot freeze.
    ///
    /// Mutations: a loop gain of 0.998 in the frozen network loses 4 dB; an input
    /// left open raises the level.
    pub(crate) fn freeze<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let second = rate.get() as usize;
        let level = |out: &[Vec<f32>], from: usize| 10.0 * energy(out, from, second / 5).log10();
        for (thawed, frozen) in d.freeze_pairs() {
            let (mut k, _) = built(d, &thawed, rate);
            let shape = widths(d);
            render(&mut k, shape, &noise(5, 3 * second, 0.5), 64);
            k.retarget(&d.design(&frozen, rate));
            let held = render(&mut k, shape, &vec![0.0; 10 * second], 64);
            let (early, late) = (level(&held, second), level(&held, 9 * second));
            if !early.is_finite() || (late - early).abs() > 0.25 {
                return Err(format!(
                    "the frozen level went from {early:.2} dB to {late:.2} dB in 8 s"
                ));
            }
            let loud = noise(6, second, 1.0);
            let more = render(&mut k, shape, &loud, 64);
            let after = render(&mut k, shape, &vec![0.0; second], 64);
            let _ = more;
            let settled = level(&after, 0);
            if (settled - late).abs() > 0.25 {
                return Err(format!(
                    "input reached a frozen tail: {late:.2} dB before, {settled:.2} dB after"
                ));
            }
        }
        Ok(())
    }

    /// Width: at its natural setting the channels are decorrelated and at none
    /// they are one (correlation over 0.999). Vacuous for a design with no width
    /// control.
    ///
    /// Mutation: the realization ignores the width.
    pub(crate) fn width<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let corr = |c: &D::Controls| -> f64 {
            let (mut k, _) = built(d, c, rate);
            let out = render(&mut k, widths(d), &noise(11, 1 << 16, 1.0), 64);
            let dot = |a: &[f32], b: &[f32]| -> f64 {
                a.iter()
                    .zip(b)
                    .map(|(&x, &y)| f64::from(x) * f64::from(y))
                    .sum()
            };
            dot(&out[0], &out[1])
                / (dot(&out[0], &out[0]) * dot(&out[1], &out[1]))
                    .sqrt()
                    .max(f64::MIN_POSITIVE)
        };
        for (natural, mono) in d.width_pairs() {
            let (n, m) = (corr(&natural), corr(&mono));
            if n.abs() >= 0.3 || m < 0.999 {
                return Err(format!(
                    "correlation {n:.3} at natural width, {m:.4} at none"
                ));
            }
        }
        Ok(())
    }

    /// The wet gain scales the sound: `b` is `a` at `ratio` times the level to
    /// 2%, and exactly silent when the ratio is 0. Vacuous for a design with no
    /// wet gain.
    ///
    /// Mutation: the realization ignores the wet gain.
    pub(crate) fn wet_gain<D: RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let x = noise(12, 1 << 14, 1.0);
        for (a, b, ratio) in d.wet_pairs() {
            let run = |c: &D::Controls| {
                let (mut k, _) = built(d, c, rate);
                render(&mut k, widths(d), &x, 64)
            };
            let (oa, ob) = (run(&a), run(&b));
            let (ea, eb) = (rms(&oa, 0, x.len()), rms(&ob, 0, x.len()));
            let ok = if ratio == 0.0 {
                ob.iter().flatten().all(|&s| s == 0.0)
            } else {
                (eb / ea - f64::from(ratio)).abs() <= 0.02 * f64::from(ratio)
            };
            if !ok {
                return Err(format!("wet gain: level ratio {}, wanted {ratio}", eb / ea));
            }
        }
        Ok(())
    }
}

/// Makes one `#[test]` per case of the realization contract for `$design`, an
/// expression of a [`RealizationCase`] type.
macro_rules! realization_contract {
    ($design:expr) => {
        #[test]
        fn realization_contract_rt60() {
            $crate::contract::realization::cases::rt60(&$design).unwrap();
        }
        #[test]
        fn realization_contract_band_order() {
            $crate::contract::realization::cases::band_order(&$design).unwrap();
        }
        #[test]
        fn realization_contract_tail_covers() {
            $crate::contract::realization::cases::tail_covers(&$design).unwrap();
        }
        #[test]
        fn realization_contract_arrival() {
            $crate::contract::realization::cases::arrival(&$design).unwrap();
        }
        #[test]
        fn realization_contract_latency() {
            $crate::contract::realization::cases::latency(&$design).unwrap();
        }
        #[test]
        fn realization_contract_wet_only() {
            $crate::contract::realization::cases::wet_only(&$design).unwrap();
        }
        #[test]
        fn realization_contract_decorrelated() {
            $crate::contract::realization::cases::decorrelated(&$design).unwrap();
        }
        #[test]
        fn realization_contract_stability() {
            $crate::contract::realization::cases::stability(&$design).unwrap();
        }
        #[test]
        fn realization_contract_click_free() {
            $crate::contract::realization::cases::click_free(&$design).unwrap();
        }
        #[test]
        fn realization_contract_line_lengths() {
            $crate::contract::realization::cases::line_lengths(&$design).unwrap();
        }
        #[test]
        fn realization_contract_pre_delay() {
            $crate::contract::realization::cases::pre_delay(&$design).unwrap();
        }
        #[test]
        fn realization_contract_size() {
            $crate::contract::realization::cases::size(&$design).unwrap();
        }
        #[test]
        fn realization_contract_diffusion() {
            $crate::contract::realization::cases::diffusion(&$design).unwrap();
        }
        #[test]
        fn realization_contract_freeze() {
            $crate::contract::realization::cases::freeze(&$design).unwrap();
        }
        #[test]
        fn realization_contract_width() {
            $crate::contract::realization::cases::width(&$design).unwrap();
        }
        #[test]
        fn realization_contract_wet_gain() {
            $crate::contract::realization::cases::wet_gain(&$design).unwrap();
        }
        #[test]
        fn realization_contract_reset() {
            $crate::contract::realization::cases::reset(&$design).unwrap();
        }
        #[test]
        fn realization_contract_fresh() {
            $crate::contract::realization::cases::fresh(&$design).unwrap();
        }
        #[test]
        fn realization_contract_block_independent() {
            $crate::contract::realization::cases::block_independent(&$design).unwrap();
        }
    };
}
pub(crate) use realization_contract;

// ---- the defective realizations the cases must fail ------------------------------------

/// One thing wrong with a realization, added around a real one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Defect {
    /// `retarget` scales every loop gain by this factor.
    LoopGain(f32),
    /// `retarget` drops the damping: every band decays like the lowest.
    NoDamping,
    /// The output is one frame late and `latency` does not say so.
    LateByOne,
    /// `latency` claims one frame more than the realization delays.
    FalseLatency,
    /// The input is added to the first output channel.
    DryLeak,
    /// The second output channel is a copy of the first.
    MonoOutput,
    /// From the second `retarget` on, the output is scaled by this factor, whole.
    UnslewedGain(f32),
    /// `reset` does nothing.
    NoReset,
    /// `fresh` keeps the state.
    FreshKeepsState,
    /// The output is scaled by `1 + 1e-6 * block length`.
    BlockDependent,
    /// Nothing wrong in the wrapper (the realization itself may be tweaked).
    None,
    /// The output is scaled by a gain that grows 0.01% a frame, for ever.
    Runaway,
    /// `retarget` makes the last line 5% longer than the network says.
    LastLineLong,
    /// `retarget` drops the pre-delay.
    PreDelayIgnored,
    /// `retarget` gives every line one length, whatever the size.
    FixedLines,
    /// `retarget` mixes no line into another.
    NoMixing,
    /// `retarget` opens the input of every line, frozen or not.
    OpenInput,
    /// `retarget` sets the stereo width to its natural one.
    WidthIgnored,
    /// `retarget` sets the wet gain to 0 dB.
    WetIgnored,
}

/// A real realization with one [`Defect`].
#[derive(Clone, Debug)]
pub(crate) struct Flawed<K> {
    inner: K,
    defect: Defect,
    retargets: usize,
    /// The last frame of the previous block, per channel, for `LateByOne`.
    held: [f32; 8],
    /// The gain of `Runaway`.
    runaway: f32,
}

impl<K: Effect<Network, Bounds = Bounds> + Clone> Flawed<K> {
    /// Retargets the inner realization to `network` as `edit` leaves it.
    fn edited(&mut self, network: &Network, edit: impl FnOnce(&mut Network)) {
        let mut n = Box::new(network.clone());
        edit(&mut n);
        self.inner.retarget(&n);
    }
}

impl<K: Effect<Network, Bounds = Bounds> + Clone> Realization<Network> for Flawed<K> {
    type Bounds = Bounds;

    fn prepare(&mut self, bounds: &Bounds, setup: &Setup) {
        self.inner.prepare(bounds, setup);
    }

    fn retarget(&mut self, network: &Network) {
        self.retargets += 1;
        match self.defect {
            Defect::LoopGain(factor) => {
                let mut n = network.clone();
                map_absorption(&mut n, |a| match a {
                    Absorption::OnePole { gain, damp } => Absorption::OnePole {
                        gain: gain * factor,
                        damp,
                    },
                    Absorption::Gain(g) => Absorption::Gain(g * factor),
                    Absorption::Bands {
                        low,
                        mid,
                        high,
                        crossover,
                    } => Absorption::Bands {
                        low: low * factor,
                        mid: mid * factor,
                        high: high * factor,
                        crossover,
                    },
                });
                self.inner.retarget(&n);
            }
            Defect::NoDamping => {
                let mut n = network.clone();
                map_absorption(&mut n, |a| match a {
                    Absorption::OnePole { gain, .. } => Absorption::OnePole { gain, damp: 0.0 },
                    // Every band like the lowest.
                    Absorption::Bands { low, crossover, .. } => Absorption::Bands {
                        low,
                        mid: low,
                        high: low,
                        crossover,
                    },
                    a => a,
                });
                self.inner.retarget(&n);
            }
            Defect::LastLineLong => self.edited(network, |n| {
                map_fdn(n, |f| {
                    if let Some(l) = f.lines.as_mut_slice().last_mut() {
                        l.length *= 1.05;
                    }
                })
            }),
            Defect::PreDelayIgnored => self.edited(network, |n| n.pre_delay = Samples::ZERO),
            Defect::FixedLines => self.edited(network, |n| {
                map_fdn(n, |f| {
                    f.lines
                        .as_mut_slice()
                        .iter_mut()
                        .for_each(|l| l.length = 1500.0)
                })
            }),
            Defect::NoMixing => self.edited(network, |n| {
                map_fdn(n, |f| f.matrix = crate::network::Matrix::Diagonal)
            }),
            Defect::OpenInput => self.edited(network, |n| {
                map_fdn(n, |f| f.input.as_mut_slice().fill(0.35))
            }),
            Defect::WidthIgnored => {
                self.edited(network, |n| n.width = tutti_types::StereoWidth::NATURAL)
            }
            Defect::WetIgnored => self.edited(network, |n| n.wet = Db(0.0)),
            _ => self.inner.retarget(network),
        }
    }

    fn reset(&mut self) {
        if self.defect != Defect::NoReset {
            self.inner.reset();
        }
    }

    fn latency(&self) -> Samples {
        let l = self.inner.latency();
        if self.defect == Defect::FalseLatency {
            Samples(l.get() + 1)
        } else {
            l
        }
    }

    fn effort(&self) -> crate::Effort {
        self.inner.effort()
    }

    fn fresh(&self) -> Self {
        if self.defect == Defect::FreshKeepsState {
            return self.clone();
        }
        Self {
            inner: self.inner.fresh(),
            defect: self.defect,
            retargets: self.retargets,
            held: [0.0; 8],
            runaway: 1.0,
        }
    }
}

impl<K: Effect<Network, Bounds = Bounds> + Clone> Effect<Network> for Flawed<K> {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        self.inner.process(input, output);
        match self.defect {
            Defect::LateByOne => {
                for (c, ch) in output.iter_mut().enumerate() {
                    for s in ch.iter_mut() {
                        std::mem::swap(s, &mut self.held[c]);
                    }
                }
            }
            Defect::DryLeak => {
                for (o, i) in output[0].iter_mut().zip(input[0]) {
                    *o += i;
                }
            }
            Defect::MonoOutput => {
                let (first, rest) = output.split_at_mut(1);
                rest[0].copy_from_slice(first[0]);
            }
            Defect::UnslewedGain(factor) if self.retargets >= 2 => {
                output
                    .iter_mut()
                    .flat_map(|c| c.iter_mut())
                    .for_each(|s| *s *= factor);
            }
            Defect::BlockDependent => {
                let len = output[0].len() as f32;
                output
                    .iter_mut()
                    .flat_map(|c| c.iter_mut())
                    .for_each(|s| *s *= 1.0 + 1e-6 * len);
            }
            Defect::Runaway => {
                for i in 0..output[0].len() {
                    self.runaway *= 1.0001;
                    for ch in output.iter_mut() {
                        ch[i] *= self.runaway;
                    }
                }
            }
            _ => {}
        }
    }
}

/// A design whose realization has one [`Defect`].
#[derive(Clone)]
pub(crate) struct Defective<D: DesignCase> {
    pub(crate) inner: D,
    pub(crate) defect: Defect,
    /// Applied to the realization as it is built: a defect in the realization's own state.
    pub(crate) tweak: fn(&mut D::Realization),
}

impl<D: DesignCase> Design for Defective<D> {
    type Controls = D::Controls;
    type Model = Network;
    type Bounds = Bounds;
    type Range = MaxDecay;
    type Realization = Flawed<D::Realization>;

    fn blank(&self, rate: SampleRate) -> Network {
        Network::new(rate)
    }

    fn measure(&self, model: &Network, floor: Db) -> Bounds {
        self.inner.measure(model, floor)
    }

    fn channels(&self) -> Channels {
        self.inner.channels()
    }

    fn design_into(&self, c: &D::Controls, rate: SampleRate, out: &mut Network) {
        self.inner.design_into(c, rate, out);
    }

    fn bounds(&self, ceiling: &Ceiling<MaxDecay>) -> Bounds {
        self.inner.bounds(ceiling)
    }

    fn realization(&self) -> Flawed<D::Realization> {
        let mut inner = self.inner.realization();
        (self.tweak)(&mut inner);
        Flawed {
            inner,
            defect: self.defect,
            retargets: 0,
            held: [0.0; 8],
            runaway: 1.0,
        }
    }
}

impl<D: DesignCase> DesignCase for Defective<D> {
    fn rates(&self) -> Vec<SampleRate> {
        self.inner.rates()
    }
    fn reachable(&self) -> Vec<D::Controls> {
        self.inner.reachable()
    }
    fn hostile(&self) -> Vec<D::Controls> {
        self.inner.hostile()
    }
    fn wet_only(&self) -> bool {
        self.inner.wet_only()
    }
    fn pre_delay_pairs(&self) -> Vec<(D::Controls, D::Controls, tutti_types::Seconds)> {
        self.inner.pre_delay_pairs()
    }
    fn size_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.size_pairs()
    }
    fn diffusion_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.diffusion_pairs()
    }
}

impl<D: RealizationCase> RealizationCase for Defective<D> {
    fn tolerance(&self) -> f32 {
        self.inner.tolerance()
    }
    fn probes(&self) -> Vec<D::Controls> {
        self.inner.probes()
    }
    fn extremes(&self) -> Vec<D::Controls> {
        self.inner.extremes()
    }
    fn steps(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.steps()
    }
    fn peak_bound(&self, c: &D::Controls, n: &Network) -> f32 {
        self.inner.peak_bound(c, n)
    }
    fn tone(&self, band: Band, rate: SampleRate) -> f32 {
        self.inner.tone(band, rate)
    }
    fn fit_depth(&self) -> (f64, f64) {
        self.inner.fit_depth()
    }
    fn static_probes(&self) -> Vec<D::Controls> {
        self.inner.static_probes()
    }
    fn freeze_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.freeze_pairs()
    }
    fn width_pairs(&self) -> Vec<(D::Controls, D::Controls)> {
        self.inner.width_pairs()
    }
    fn wet_pairs(&self) -> Vec<(D::Controls, D::Controls, f32)> {
        self.inner.wet_pairs()
    }
}
