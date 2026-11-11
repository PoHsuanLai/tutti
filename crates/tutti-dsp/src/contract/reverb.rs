//! The `reverb` group: the cases of a [`Network`], the controls a reverb has and
//! a filter does not.

use tutti_types::{Amplitude, Db, Hz, SampleRate, Samples, Seconds};

use super::core::{check_vacuity, every};
use super::harness::{Burst, WINDOW, built, burst, energy, noise, rate_of, render, rms, widths};
use super::{Hook, RealizationCase, ReverbCase};
use crate::{Absorption, Band, Bounds, Lti, Network, Realization, Ring, Stage, System};

fn close(a: f32, b: f32, relative: f32) -> bool {
    a == b || (a - b).abs() <= relative * a.abs().max(b.abs())
}

/// The helpers the RT60 cases share.
/// How many frames of burst plateau make `lines` trips of the longest line.
pub(super) fn burst_frames(bounds: &Bounds, lines: f32) -> usize {
    (lines * bounds.line) as usize
}

/// The RT60 of the decay after the burst: a least squares line through the
/// energy of the windows between 40 and 65 dB below the plateau's, `60 / -slope`.
/// Deep, because a bank of lines of different length decays at the slowest
/// line's rate in the end, and not deeper, because a realization flushes values
/// under 1e-6 (Freeverb's does, which the network does not model) and a deeper
/// fit measures the flush.
pub(super) fn measured_rt60(
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

/// Frames of silence after a burst that reach 65 dB down at the slowest rate
/// the network gives in any band, with room (a realization whose bands decay slower
/// than its network says is measured, not cut short).
pub(super) fn rt60_frames(
    network: &Network,
    rate: SampleRate,
    depth: f64,
) -> Result<usize, String> {
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

/// The design cases of the `reverb` group.
pub(super) mod design {
    use super::*;

    /// `rt60(band)` equals the RT60 the controls ask for, per
    /// `g = 10^(-3m/(RT60 fs))`, within 1e-3 relative. The design must ask for
    /// at least one.
    pub fn rt60_matches<D: ReverbCase>(d: &D) -> Result<(), String> {
        let mut asked = 0;
        for rate in d.rates() {
            for c in d.reachable() {
                let n = d.design(&c, rate);
                for band in Band::ALL {
                    if let Some(want) = d.requested_rt60(&c, rate, band) {
                        asked += 1;
                        let got = n.rt60(band);
                        if !close(got.to_f32(), want.to_f32(), 1e-3) {
                            return Err(format!(
                                "{band:?} at {rate:?}: rt60 {got:?}, asked {want:?}"
                            ));
                        }
                    }
                }
            }
        }
        if asked == 0 {
            Err("no control asks for an RT60".into())
        } else {
            Ok(())
        }
    }

    /// The bands' RT60s are ordered as the controls ask: for every pair of
    /// bands, the order of the requested figures is the order of the network's.
    pub fn band_order<D: ReverbCase>(d: &D) -> Result<(), String> {
        let mut checked = 0;
        for rate in d.rates() {
            for c in d.reachable() {
                let n = d.design(&c, rate);
                let want: Vec<Option<Seconds>> = Band::ALL
                    .iter()
                    .map(|&b| d.requested_rt60(&c, rate, b))
                    .collect();
                if want.iter().any(Option::is_none) {
                    continue;
                }
                let want: Vec<f32> = want.iter().flatten().map(|w| w.to_f32()).collect();
                let got: Vec<f32> = Band::ALL.iter().map(|&b| n.rt60(b).to_f32()).collect();
                for (i, j) in [(0, 1), (1, 2), (0, 2)] {
                    checked += 1;
                    let ordered = |v: &[f32]| {
                        if close(v[i], v[j], 1e-3) {
                            std::cmp::Ordering::Equal
                        } else {
                            v[i].total_cmp(&v[j])
                        }
                    };
                    if ordered(&want) != ordered(&got) {
                        return Err(format!(
                            "bands {i},{j} at {rate:?}: asked {want:?}, got {got:?}"
                        ));
                    }
                }
            }
        }
        if checked == 0 {
            Err("no control asks for band RT60s".into())
        } else {
            Ok(())
        }
    }

    /// The tail is non-decreasing in decay and in size, a lower floor is longer,
    /// and a pre-delay adds its length to the tail and no more.
    pub fn tail_monotone<D: ReverbCase>(d: &D) -> Result<(), String> {
        let floor = Db(-60.0);
        for rate in d.rates() {
            let tail = |c: &D::Controls, f: Db| d.design(c, rate).tail(f);
            for (a, b) in d.decay_pairs().into_iter().chain(d.size_pairs()) {
                let (ta, tb) = (tail(&a, floor), tail(&b, floor));
                match (ta, tb) {
                    (Ring::Finite(x), Ring::Finite(y)) if y.get() >= x.get() * (1.0 - 1e-6) => {}
                    (_, Ring::Unbounded) => {}
                    _ => return Err(format!("tail fell from {ta:?} to {tb:?} at {rate:?}")),
                }
            }
            for c in d.reachable() {
                let (t40, t60, t90) = (tail(&c, Db(-40.0)), tail(&c, floor), tail(&c, Db(-90.0)));
                if !(t60.covers(t40) && t90.covers(t60)) {
                    return Err(format!("floors not ordered: {t40:?} {t60:?} {t90:?}"));
                }
            }
            for (without, with, by) in d.pre_delay_pairs() {
                let (Ring::Finite(x), Ring::Finite(y)) =
                    (tail(&without, floor), tail(&with, floor))
                else {
                    return Err("an unbounded tail in a pre-delay pair".into());
                };
                let added = y.get() - x.get();
                let want = f64::from(by.to_samples(rate).get() as u32) / rate.get();
                if (added - want).abs() > 2e-6 + 1e-6 * y.get() {
                    return Err(format!("pre-delay added {added} s, not {want} s"));
                }
            }
        }
        Ok(())
    }

    /// The network is lossless exactly when the controls freeze it, and its tail
    /// is unbounded then and only then (a freeze is still stable). Vacuous in
    /// the lossless direction for a design that cannot freeze: it still asserts
    /// that no control gives a lossless loop.
    pub fn lossless_iff_frozen<D: ReverbCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in d.reachable() {
                let n = d.design(&c, rate);
                let frozen = d.frozen(&c);
                if n.is_lossless() != frozen {
                    return Err(format!("lossless {} but frozen {frozen}", n.is_lossless()));
                }
                if (n.tail(Db(-60.0)) == Ring::Unbounded) != frozen {
                    return Err(format!("tail {:?} but frozen {frozen}", n.tail(Db(-60.0))));
                }
                if !n.is_stable() {
                    return Err("unstable".into());
                }
            }
        }
        Ok(())
    }

    /// Modal density is positive, finite and grows with size; echo density never
    /// falls with time and grows with diffusion: orderings, not absolute figures.
    pub fn density<D: ReverbCase>(d: &D) -> Result<(), String> {
        let at = |s: f32| Seconds::from_f32(s);
        for rate in d.rates() {
            for c in d.reachable() {
                let n = d.design(&c, rate);
                let m = n.modal_density().get();
                if !(m.is_finite() && m > 0.0) {
                    return Err(format!("modal density {m}"));
                }
                let mut last = 0.0;
                for t in [0.001, 0.01, 0.1, 1.0, 10.0] {
                    let e = n.echo_density(at(t)).get();
                    if e < last {
                        return Err(format!("echo density fell to {e} at {t} s"));
                    }
                    last = e;
                }
            }
            for (a, b) in d.size_pairs() {
                let (ma, mb) = (
                    d.design(&a, rate).modal_density(),
                    d.design(&b, rate).modal_density(),
                );
                if mb.get() <= ma.get() {
                    return Err(format!("modal density {ma:?} -> {mb:?} for a bigger room"));
                }
            }
            for (a, b) in d.diffusion_pairs() {
                let (ea, eb) = (
                    d.design(&a, rate).echo_density(at(1.0)),
                    d.design(&b, rate).echo_density(at(1.0)),
                );
                if eb.get() <= ea.get() {
                    return Err(format!("echo density {ea:?} -> {eb:?} for more diffusion"));
                }
            }
        }
        Ok(())
    }

    /// `is_stable` refuses hostile absorptions and lengths fed to it directly:
    /// the design clamps its controls, so the design-level `stable` case cannot
    /// reach them. Every recirculating stage of a sample of the design's
    /// networks has each line set, in turn, to a one-pole with a pole at or past
    /// the unit circle (or not a number), a negative pole that peaks above its
    /// gain, a gain of 1.5, NaN, infinity or below zero, and a length of 0, a
    /// negative, NaN or infinite one; none may be stable.
    pub fn stable_refuses_hostile<D: ReverbCase>(d: &D) -> Result<(), String> {
        let pole = |damp| Absorption::OnePole {
            gain: Amplitude(0.9),
            damp,
        };
        let bands = Absorption::Bands {
            low: Amplitude(0.5),
            mid: Amplitude(0.5),
            high: Amplitude(1.5),
            crossover: [Hz(250.0), Hz(4000.0)],
        };
        let hostile = [
            pole(2.0),
            pole(-2.0),
            pole(1.0),
            pole(f32::NAN),
            pole(f32::INFINITY),
            pole(-0.5),
            Absorption::Gain(Amplitude(1.5)),
            Absorption::Gain(Amplitude(f32::NAN)),
            Absorption::Gain(Amplitude(f32::INFINITY)),
            Absorption::Gain(Amplitude(-0.1)),
            bands,
        ];
        let rate = d.rates()[0];
        let mut tried = 0;
        for c in d.reachable().into_iter().step_by(17) {
            let base = d.design(&c, rate);
            for (i, stage) in base.stages.iter().enumerate() {
                let Stage::Fdn(f) = stage else { continue };
                for a in hostile {
                    let mut n = base.clone();
                    let Stage::Fdn(f) = &mut n.stages.as_mut_slice()[i] else {
                        unreachable!()
                    };
                    f.lines
                        .as_mut_slice()
                        .iter_mut()
                        .for_each(|l| l.absorption = a);
                    tried += 1;
                    if n.is_stable() {
                        return Err(format!("{a:?} on every line is called stable"));
                    }
                }
                for length in [0.0, -1.0, f32::NAN, f32::INFINITY] {
                    let mut n = base.clone();
                    let Stage::Fdn(f) = &mut n.stages.as_mut_slice()[i] else {
                        unreachable!()
                    };
                    f.lines.as_mut_slice()[0].length = length;
                    tried += 1;
                    if n.is_stable() {
                        return Err(format!("a line of length {length} is called stable"));
                    }
                }
                let _ = f;
            }
        }
        if tried == 0 {
            Err("no recirculating stage to try".into())
        } else {
            Ok(())
        }
    }

    /// A design that says it is wet-only has no path from input to output
    /// shorter than a frame.
    pub fn no_direct_path<D: ReverbCase>(d: &D) -> Result<(), String> {
        if !d.wet_only() {
            return Ok(());
        }
        for rate in d.rates() {
            for c in every(d) {
                let n = d.design(&c, rate);
                if n.has_direct_path() {
                    return Err(format!("direct path, shortest {} frames", n.min_delay()));
                }
            }
        }
        Ok(())
    }

    /// `pre_delay` is `round(pre_delay seconds * rate)` frames, for every control
    /// in range (a hostile one is clamped to the design's own range).
    pub fn pre_delay<D: ReverbCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in d.reachable() {
                let n = d.design(&c, rate);
                let asked = d.pre_delay(&c);
                let want = if asked.get().is_finite() && asked.get() > 0.0 {
                    Samples((asked.get() * rate.get()).round() as usize)
                } else {
                    Samples::ZERO
                };
                if n.pre_delay != want {
                    return Err(format!(
                        "pre-delay {} frames, asked {want} ({asked:?})",
                        n.pre_delay
                    ));
                }
            }
        }
        Ok(())
    }

    /// Every absorption of every reachable design peaks at or under 1: the kernel
    /// rescales a band set whose response exceeds 1 anywhere (to peak at 0.9999),
    /// so a model that carried such gains would describe a loop the kernel does not
    /// run. The design applies that scaling itself (`FdnDesign` writes the scaled
    /// gains into its network), so the network is what runs.
    pub fn no_rescale<D: ReverbCase>(d: &D) -> Result<(), String> {
        let mut checked = 0;
        for rate in d.rates() {
            for c in d.reachable() {
                let n = d.design(&c, rate);
                for stage in n.stages.iter() {
                    let Stage::Fdn(f) = stage else { continue };
                    for line in f.lines.iter() {
                        let peak = line.absorption.peak(rate);
                        checked += 1;
                        // The kernel scales to 0.9999 on a grid of 97 points, which
                        // reads up to 0.12% under the true peak (measured): one percent.
                        if peak > 1.01 {
                            return Err(format!(
                                "an absorption peaks at {peak}: the realization would rescale it"
                            ));
                        }
                    }
                }
            }
        }
        if checked == 0 {
            return Err("no recirculating line to check".into());
        }
        Ok(())
    }

    /// The reverb hooks of the design level return something, or the design says
    /// why they cannot.
    pub fn reverb_vacuity<D: ReverbCase>(d: &D) -> Result<(), String> {
        check_vacuity(
            d.vacant(),
            &[
                (Hook::DecayPairs, d.decay_pairs().len()),
                (Hook::SizePairs, d.size_pairs().len()),
                (Hook::DiffusionPairs, d.diffusion_pairs().len()),
                (Hook::PreDelayPairs, d.pre_delay_pairs().len()),
            ],
        )
    }
}

/// The realization cases of the `reverb` group.
pub(super) mod realization {
    use super::*;

    /// A tone burst in each band, stopped; the energy decay after it has the RT60
    /// the network's `rt60(band)` says, within the realization's tolerance (measured
    /// from 40 to 65 dB down, where the slowest line governs).
    pub fn rt60<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let tol = d.tolerance();
        let mut measured = 0;
        for c in d.static_probes() {
            for band in Band::ALL {
                let (mut k, network) = built(d, &c, rate);
                let ceiling = d.ceiling(rate);
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
    pub fn band_order<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let tol = d.tolerance();
        let mut ordered = 0;
        for c in d.static_probes() {
            let ceiling = d.ceiling(rate);
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

    /// The realization is wet only: with noise in from the first frame, every output
    /// channel is exactly zero until the network's shortest path has been
    /// travelled, so none of the input reaches the output.
    pub fn wet_only<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
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
    pub fn decorrelated<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
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

    /// Every line of a first recirculating stage that feeds the output through
    /// the stages after it, with nothing but series all-passes after: with the
    /// loop gain high, an impulse's first echo of line `i` lands at
    /// `pre_delay + length_i` and is the input gain's size (a Freeverb all-pass
    /// chain of four passes the first sample at full size). Pins every line
    /// length, not only the shortest that `arrival` finds. Vacuous (an error) for
    /// a network of another shape.
    pub fn line_lengths<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
        use crate::reverb::network::Stage;
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

    /// A pre-delay moves the first sound by exactly its frames, and `latency()`
    /// stays what it was: pre-delay is part of the wet sound, never latency.
    /// Vacuous for a design with no pre-delay control.
    pub fn pre_delay<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
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
    pub fn size<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        for (a, b) in d.size_pairs() {
            let (fa, fb) = (first_sound(d, &a, rate), first_sound(d, &b, rate));
            if fa.zip(fb).is_none_or(|(x, y)| y <= x) {
                return Err(format!("a bigger room sounded at {fb:?}, not after {fa:?}"));
            }
        }
        Ok(())
    }

    /// More diffusion measures denser: in the window from 0.15 to 0.3 s of an
    /// impulse's response the denser controls fill at least 0.15 more of the
    /// frames. Vacuous for a design with no diffusion control.
    pub fn diffusion<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
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
    pub fn freeze<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
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
    pub fn width<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
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
    pub fn wet_gain<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
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

    /// The reverb hooks of the realization level return something, or the design
    /// says why they cannot.
    pub fn reverb_vacuity<D: ReverbCase + RealizationCase>(d: &D) -> Result<(), String> {
        check_vacuity(
            d.vacant(),
            &[
                (Hook::FreezePairs, d.freeze_pairs().len()),
                (Hook::WidthPairs, d.width_pairs().len()),
                (Hook::WetPairs, d.wet_pairs().len()),
            ],
        )
    }

    /// The first frame any channel sounds for a unit impulse at frame 0 of the
    /// network `c` designs, in `frames`.
    fn first_sound<D: RealizationCase>(d: &D, c: &D::Controls, rate: SampleRate) -> Option<usize> {
        let (mut k, network) = built(d, c, rate);
        let frames = d.musical_delay(&network) + 4000;
        let mut x = vec![0.0f32; frames];
        x[0] = 1.0;
        render(&mut k, widths(d), &x, 64)
            .iter()
            .filter_map(|ch| ch.iter().position(|&s| s != 0.0))
            .min()
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
}
