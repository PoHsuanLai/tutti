//! The kernel contract (design 022 section 8.2): cases every [`Kernel`] must
//! pass against the [`Network`] of its [`Design`], measured on its output.
//!
//! The design contract ([`super::contract`]) holds a network to the analytic
//! figures its controls ask for; this one holds the kernel to the network. A
//! kernel implements [`KernelCase`] (its tolerance, the controls to probe and
//! the bound on its output) and runs [`kernel_contract!`], which makes one
//! `#[test]` per case. Every figure is the network's own query (`rt60`, `tail`,
//! `min_delay`, `latency`, `has_direct_path`) or a property of the render
//! (finite, bounded, silent before the first arrival, bit-identical after
//! `reset`), never another implementation's output.
//!
//! **Each case runs against a deliberately broken kernel it must fail**: a
//! [`Defective`] design hands out a [`Flawed`] kernel, a real kernel with one
//! [`Defect`] added around it, and the test beside the suite asserts the case
//! reports the failure (a case that cannot fail proves nothing).
//!
//! What the suite leaves to the design it runs over: a kernel with no freeze,
//! pre-delay, width or wet control has nothing to say in those directions.
//! Freeverb's network never sets a pre-delay, a width or a wet gain, so those
//! directions are vacuous for it (the arrival case still places its pre-delay,
//! zero, and the lossless case is the design contract's). Allocation-freedom of
//! `retarget` and `process` needs a counting allocator, which `unsafe` forbids
//! in this crate's lib tests: `tests/kernel_alloc.rs` runs it.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tutti_types::{Db, MaxBlock, SampleRate, Samples};

use super::contract::{DesignCase, map_absorption};
use super::{Absorption, Band, Bounds, Ceiling, Channels, Design, Kernel, Network, Ring, Setup};

/// The longest block the cases prepare for.
const MAX_BLOCK: usize = 512;
/// The window the energy of a decay is measured in, in frames.
const WINDOW: usize = 1024;

/// A design plus what the kernel contract needs to know about its kernel.
pub(crate) trait KernelCase: DesignCase
where
    Self::Kernel: Clone,
{
    /// The relative error the kernel's measured RT60 may have against the
    /// network's `rt60` (the kernel declares it: design 022 section 8.2).
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

/// A prepared kernel retargeted to `controls`, and its network.
fn built<D: Design>(d: &D, c: &D::Controls, rate: SampleRate) -> (D::Kernel, Network) {
    let ceiling = Ceiling::at(rate).expect("a positive finite rate");
    let mut k = d.kernel();
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
fn render<K: Kernel>(
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

/// A tone burst: the band's test frequency (DC, a quarter of the rate, Nyquist:
/// the three points [`Band`] names for a response that is not given by bands)
/// for `plateau` frames between raised-cosine fades of [`FADE`] frames, then
/// `after` frames of silence. The fades keep the burst to its band: an abrupt
/// start or stop leaks energy into the low frequencies, which decay slowest and
/// would govern the deep part of a high band's decay.
struct Burst {
    signal: Vec<f32>,
    /// Where the plateau ends and the fade-out begins.
    plateau_end: usize,
    /// Where the input is silent for good.
    end: usize,
}

/// The length of a burst's fades, in frames.
const FADE: usize = 4096;

fn burst(band: Band, plateau: usize, after: usize) -> Burst {
    let end = plateau + 2 * FADE;
    let mut signal = vec![0.0f32; end + after];
    for (n, s) in signal[..end].iter_mut().enumerate() {
        let tone = match band {
            Band::Low => 1.0,
            Band::Mid => [1.0, 0.0, -1.0, 0.0][n % 4],
            Band::High => {
                if n % 2 == 0 {
                    1.0
                } else {
                    -1.0
                }
            }
        };
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
/// line's rate in the end, and not deeper, because a kernel flushes values
/// under 1e-6 (Freeverb's does, which the network does not model) and a deeper
/// fit measures the flush.
fn measured_rt60(out: &[Vec<f32>], b: &Burst, rate: SampleRate) -> Result<f32, String> {
    let reference = energy(out, b.plateau_end - WINDOW, WINDOW);
    if reference <= 0.0 {
        return Err("nothing sounds before the input stops".into());
    }
    let len = out[0].len();
    let (mut n, mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    let mut start = b.end;
    while start + WINDOW <= len {
        let db = 10.0 * (energy(out, start, WINDOW) / reference).log10();
        if (-65.0..=-40.0).contains(&db) {
            let t = (start - b.end + WINDOW / 2) as f64 / rate.get();
            n += 1.0;
            sx += t;
            sy += db;
            sxx += t * t;
            sxy += t * db;
        }
        start += WINDOW;
    }
    if n < 4.0 {
        let first = 10.0 * (energy(out, b.end, WINDOW) / reference).log10();
        let last = 10.0 * (energy(out, len - WINDOW, WINDOW) / reference).log10();
        return Err(format!(
            "only {n} windows between -40 and -65 dB (first window {first:.1} dB, last {last:.1} dB)"
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
/// the network gives in any band, with room (a kernel whose bands decay slower
/// than its network says is measured, not cut short).
fn rt60_frames(network: &Network, rate: SampleRate) -> Result<usize, String> {
    let r = Band::ALL
        .iter()
        .map(|&b| network.rt60(b).get())
        .fold(0.0, f32::max);
    if r.is_finite() && r > 0.0 {
        Ok((f64::from(r) * rate.get() * 1.6) as usize + 8 * WINDOW)
    } else {
        Err(format!("a network with no finite RT60 ({r})"))
    }
}

// ---- the cases ---------------------------------------------------------------------

pub(crate) mod cases {
    use super::*;

    /// A tone burst in each band, stopped; the energy decay after it has the RT60
    /// the network's `rt60(band)` says, within the kernel's tolerance (measured
    /// from 40 to 65 dB down, where the slowest line governs).
    ///
    /// Mutation (a `Defective` test): the kernel retargets every loop gain
    /// scaled by 1.05, 39% longer to fall 60 dB.
    pub(crate) fn rt60<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
        let rate = rate_of(d);
        let tol = d.tolerance();
        let mut measured = 0;
        for c in d.probes() {
            for band in Band::ALL {
                let (mut k, network) = built(d, &c, rate);
                let ceiling = Ceiling::at(rate).expect("a rate");
                let frames = burst_frames(&d.bounds(&ceiling), 3.0);
                let b = burst(band, frames, rt60_frames(&network, rate)?);
                let out = render(&mut k, widths(d), &b.signal, 64);
                let got = measured_rt60(&out, &b, rate)?;
                let want = network.rt60(band).get();
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
    /// Mutation: the kernel ignores the damping (every band decays like the
    /// lowest).
    pub(crate) fn band_order<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
        let rate = rate_of(d);
        let tol = d.tolerance();
        let mut ordered = 0;
        for c in d.probes() {
            let ceiling = Ceiling::at(rate).expect("a rate");
            let frames = burst_frames(&d.bounds(&ceiling), 3.0);
            let mut got = [0.0f32; 3];
            let mut want = [0.0f32; 3];
            for band in Band::ALL {
                let (mut k, network) = built(d, &c, rate);
                let b = burst(band, frames, rt60_frames(&network, rate)?);
                let out = render(&mut k, widths(d), &b.signal, 64);
                got[band.index()] = measured_rt60(&out, &b, rate)?;
                want[band.index()] = network.rt60(band).get();
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
    /// reports to the graph covers what the kernel rings.
    ///
    /// Mutation: the kernel retargets every loop gain scaled by 1.03, which rings
    /// 2.4 times as long at the largest room and so past the network's tail.
    pub(crate) fn tail_covers<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
        let rate = rate_of(d);
        let mut checked = 0;
        for c in d.probes() {
            let (mut k, network) = built(d, &c, rate);
            let ceiling = Ceiling::at(rate).expect("a rate");
            let frames = burst_frames(&d.bounds(&ceiling), 12.0);
            let Ring::Finite(deepest) = network.tail(Db(-90.0)) else {
                continue;
            };
            let after = (f64::from(deepest.get()) * rate.get()) as usize + 4 * WINDOW;
            let b = burst(Band::Low, frames, after);
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
                let allowed = (f64::from(tail.get()) * rate.get()).ceil() as usize;
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

    /// A unit impulse arrives first exactly `pre_delay + min_delay + latency`
    /// frames in (the network's own figures and the kernel's declared latency),
    /// on the earliest channel, and on no channel before.
    ///
    /// Mutations: the kernel's output delayed by one frame (its `latency` still
    /// says the same); a `latency` that claims one frame more than the kernel
    /// delays.
    pub(crate) fn arrival<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
        for rate in d.rates() {
            for c in d.probes() {
                let (mut k, network) = built(d, &c, rate);
                let expected =
                    network.pre_delay.get() + network.min_delay() as usize + k.latency().get();
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

    /// The kernel declares the network's own latency (zero for a time-domain
    /// network, a partition for a FIR), never a pre-delay or a tap spacing.
    ///
    /// Mutation: a kernel that declares one frame more than it has.
    pub(crate) fn latency<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
        for rate in d.rates() {
            for c in d.probes() {
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

    /// The kernel is wet only: with noise in from the first frame, every output
    /// channel is exactly zero until the network's shortest path has been
    /// travelled, so none of the input reaches the output.
    ///
    /// Mutation: the kernel adds its input to its first output.
    pub(crate) fn wet_only<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
        if !d.wet_only() {
            return Ok(());
        }
        for rate in d.rates() {
            for c in d.probes() {
                let (mut k, network) = built(d, &c, rate);
                let silent =
                    network.pre_delay.get() + network.min_delay() as usize + k.latency().get();
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
    /// Mutation: the kernel copies its first output channel onto its second.
    pub(crate) fn decorrelated<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
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
    pub(crate) fn stability<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
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

    /// A retarget mid-tail does not click: noise through the kernel, the controls
    /// stepped from `a` to `b` at a block edge, and the largest sample-to-sample
    /// jump after the step is within 1.5 times the larger of the two steady
    /// states' (each measured on a kernel left at its controls).
    ///
    /// Mutation: the kernel scales its output by 4 from its second retarget on, a
    /// gain applied unslewed.
    pub(crate) fn click_free<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
        let rate = rate_of(d);
        let second = rate.get() as usize;
        let jump = |out: &[Vec<f32>], from: usize, to: usize| -> f32 {
            out.iter()
                .flat_map(|c| c[from..to].windows(2))
                .fold(0.0, |m, w| m.max((w[1] - w[0]).abs()))
        };
        let steady = |c: &D::Controls| -> f32 {
            let (mut k, _) = built(d, c, rate);
            let out = render(&mut k, widths(d), &noise(5, 6 * second, 0.5), 64);
            jump(&out, 4 * second, 6 * second)
        };
        let mut checked = 0;
        for (a, b) in d.steps() {
            let (mut k, _) = built(d, &a, rate);
            let shape = widths(d);
            let mut out = render(&mut k, shape, &noise(5, 3 * second, 0.5), 64);
            k.retarget(&d.design(&b, rate));
            let after = render(&mut k, shape, &noise(6, 3 * second, 0.5), 64);
            for (all, more) in out.iter_mut().zip(after) {
                all.extend(more);
            }
            let click = jump(&out, 3 * second - 1, 6 * second);
            let allowed = 1.5 * steady(&a).max(steady(&b));
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

    /// After `reset` the kernel renders bit for bit what a freshly built one
    /// does, at the same controls.
    ///
    /// Mutation: a `reset` that does nothing.
    pub(crate) fn reset<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
        let rate = rate_of(d);
        for c in d.probes() {
            let probe = noise(9, 8192, 1.0);
            let (mut used, _) = built(d, &c, rate);
            render(&mut used, widths(d), &noise(8, 20_000, 1.0), 64);
            used.reset();
            let (mut fresh, _) = built(d, &c, rate);
            if render(&mut used, widths(d), &probe, 64) != render(&mut fresh, widths(d), &probe, 64)
            {
                return Err("a reset kernel renders differently from a fresh one".into());
            }
        }
        Ok(())
    }

    /// `fresh()` of a used kernel renders bit for bit what a freshly built one
    /// does, at the same controls: state gone, coefficients kept.
    ///
    /// Mutation: a `fresh` that keeps the state (a plain clone).
    pub(crate) fn fresh<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
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
                return Err("fresh() renders differently from a freshly built kernel".into());
            }
        }
        Ok(())
    }

    /// The output does not depend on how the input is cut into blocks: blocks of
    /// 1, 17 and 64 frames render bit for bit what blocks of 512 do (a
    /// time-domain kernel; a convolver would declare its own tolerance).
    ///
    /// Mutation: a gain that depends on the block's length.
    pub(crate) fn block_independent<D: KernelCase>(d: &D) -> Result<(), String>
    where
        D::Kernel: Clone,
    {
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
}

/// Makes one `#[test]` per case of the kernel contract for `$design`, an
/// expression of a [`KernelCase`] type.
macro_rules! kernel_contract {
    ($design:expr) => {
        #[test]
        fn kernel_contract_rt60() {
            $crate::network::kernel_contract::cases::rt60(&$design).unwrap();
        }
        #[test]
        fn kernel_contract_band_order() {
            $crate::network::kernel_contract::cases::band_order(&$design).unwrap();
        }
        #[test]
        fn kernel_contract_tail_covers() {
            $crate::network::kernel_contract::cases::tail_covers(&$design).unwrap();
        }
        #[test]
        fn kernel_contract_arrival() {
            $crate::network::kernel_contract::cases::arrival(&$design).unwrap();
        }
        #[test]
        fn kernel_contract_latency() {
            $crate::network::kernel_contract::cases::latency(&$design).unwrap();
        }
        #[test]
        fn kernel_contract_wet_only() {
            $crate::network::kernel_contract::cases::wet_only(&$design).unwrap();
        }
        #[test]
        fn kernel_contract_decorrelated() {
            $crate::network::kernel_contract::cases::decorrelated(&$design).unwrap();
        }
        #[test]
        fn kernel_contract_stability() {
            $crate::network::kernel_contract::cases::stability(&$design).unwrap();
        }
        #[test]
        fn kernel_contract_click_free() {
            $crate::network::kernel_contract::cases::click_free(&$design).unwrap();
        }
        #[test]
        fn kernel_contract_reset() {
            $crate::network::kernel_contract::cases::reset(&$design).unwrap();
        }
        #[test]
        fn kernel_contract_fresh() {
            $crate::network::kernel_contract::cases::fresh(&$design).unwrap();
        }
        #[test]
        fn kernel_contract_block_independent() {
            $crate::network::kernel_contract::cases::block_independent(&$design).unwrap();
        }
    };
}
pub(crate) use kernel_contract;

// ---- the defective kernels the cases must fail ------------------------------------

/// One thing wrong with a kernel, added around a real one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Defect {
    /// `retarget` scales every loop gain by this factor.
    LoopGain(f32),
    /// `retarget` drops the damping: every band decays like the lowest.
    NoDamping,
    /// The output is one frame late and `latency` does not say so.
    LateByOne,
    /// `latency` claims one frame more than the kernel delays.
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
    /// The output is scaled by a gain that grows 0.01% a frame, for ever.
    Runaway,
}

/// A real kernel with one [`Defect`].
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

impl<K: Kernel + Clone> Kernel for Flawed<K> {
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
                    a => a,
                });
                self.inner.retarget(&n);
            }
            _ => self.inner.retarget(network),
        }
    }

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

    fn effort(&self) -> super::Effort {
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

/// A design whose kernel has one [`Defect`].
#[derive(Clone)]
pub(crate) struct Defective<D> {
    pub(crate) inner: D,
    pub(crate) defect: Defect,
}

impl<D: Design> Design for Defective<D>
where
    D::Kernel: Clone,
{
    type Controls = D::Controls;
    type Kernel = Flawed<D::Kernel>;

    fn channels(&self) -> Channels {
        self.inner.channels()
    }

    fn design_into(&self, c: &D::Controls, rate: SampleRate, out: &mut Network) {
        self.inner.design_into(c, rate, out);
    }

    fn bounds(&self, ceiling: &Ceiling) -> Bounds {
        self.inner.bounds(ceiling)
    }

    fn kernel(&self) -> Flawed<D::Kernel> {
        Flawed {
            inner: self.inner.kernel(),
            defect: self.defect,
            retargets: 0,
            held: [0.0; 8],
            runaway: 1.0,
        }
    }
}

impl<D: DesignCase> DesignCase for Defective<D>
where
    D::Kernel: Clone,
{
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
}

impl<D: KernelCase> KernelCase for Defective<D>
where
    D::Kernel: Clone,
{
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
}
