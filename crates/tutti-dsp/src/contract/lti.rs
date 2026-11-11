//! The `Lti` group: the cases of a model that is [`Lti`].

use tutti_types::{Db, Hz, SampleRate};

use super::basic::{check_vacuity, every};
use super::harness::{WINDOW, built, burst, noise, rate_of, render, rms, widths};
use super::{Hook, LtiCase, RealizationCase};
use crate::{Complex32, Complex64, Lti, RealFft, Realization, Ring, System};

/// The frames an impulse response is rendered for: up to the model's tail at
/// -100 dB, a power of two (an FFT of the response is exact), or `None` for a
/// tail that never ends.
fn ir_frames<M: System>(model: &M, rate: SampleRate) -> Option<usize> {
    let Ring::Finite(t) = model.tail(Db(-100.0)) else {
        return None;
    };
    let frames = ((t.get() * rate.get()) as usize + 4096).next_power_of_two();
    Some(frames.min(1 << 20))
}

/// The impulse response of `d` at `controls`, one `Vec` per output, and its model.
fn impulse<D: LtiCase + RealizationCase>(
    d: &D,
    controls: &D::Controls,
    rate: SampleRate,
    frames: usize,
) -> (Vec<Vec<f32>>, D::Model) {
    let (mut k, model) = built(d, controls, rate);
    // A retarget to the model it already holds changes nothing.
    k.retarget(&model);
    let mut x = vec![0.0f32; frames];
    x[0] = 1.0;
    (render(&mut k, widths(d), &x, 512), model)
}

/// The design cases of the `Lti` group.
pub(super) mod design {
    use super::*;

    /// Every reachable (and hostile) control gives a stable model.
    ///
    /// Mutation (a `Mutant` test): a gain above 1 on every line.
    pub fn stable<D: LtiCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in every(d) {
                let n = d.design(&c, rate);
                if !n.is_stable() {
                    return Err(format!(
                        "unstable at {rate:?}: spectral radius {}",
                        n.spectral_radius()
                    ));
                }
            }
        }
        Ok(())
    }

    /// The spectral radius of every reachable (and hostile) design is under 1, and
    /// at most 1 where the controls ask for a marginal system (a freeze).
    ///
    /// Mutation (run, in `reverb_design.rs`): every comb gain scaled by 1.1, which
    /// takes the loudest room over 1.
    pub fn radius<D: LtiCase>(d: &D) -> Result<(), String> {
        for rate in d.rates() {
            for c in every(d) {
                let r = d.design(&c, rate).spectral_radius();
                let ok = if d.marginal(&c) { r <= 1.0 } else { r < 1.0 };
                if !ok {
                    return Err(format!("spectral radius {r} at {rate:?}"));
                }
            }
        }
        Ok(())
    }

    /// The response at each of the family's [`anchors`](LtiCase::anchors) is the
    /// figure the family's own formula gives, within the anchor's tolerance. A
    /// design with none says so (`vacuity`).
    ///
    /// Mutation (run, in `reverb_design.rs`): the input gain scaled by 1.1 (the
    /// `FIXED_GAIN` of Freeverb) moves the gain at DC.
    pub fn anchors<D: LtiCase>(d: &D) -> Result<(), String> {
        for a in d.anchors() {
            let model = d.design(&a.controls, a.rate);
            let mut g = [Complex64::new(0.0, 0.0)];
            model.frequency_response(a.output, &[a.freq], &mut g);
            let got = g[0].norm();
            if got.is_nan() || (got - a.gain).abs() > a.tolerance * a.gain.abs() {
                return Err(format!(
                    "|H| is {got} at {:?} Hz (output {}), the family's figures say {}",
                    a.freq.get(),
                    a.output,
                    a.gain
                ));
            }
        }
        Ok(())
    }

    /// The norms are sound and not wasteful: `hinf_norm` is at most `l1_norm`, the
    /// response's peak on a grid from DC to Nyquist is under `hinf_norm` (a bound
    /// that is under the truth is wrong), and `hinf_norm` is at most
    /// [`hinf_slack`](LtiCase::hinf_slack) times the peak (a bound that far above
    /// is not what a display scales to). The slack is checked where the grid
    /// resolves the sharpest pole, and the soundness everywhere, on the first rate
    /// and every 23rd reachable design. Marginal designs, whose peak is a pole on the
    /// unit circle, are left out.
    ///
    /// Mutations (run, in `reverb_design.rs`, on a model that lies about its
    /// `hinf_norm`): halved fails the second check, times 50 the third.
    pub fn norms<D: LtiCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let mut checked = 0;
        for c in d.reachable().into_iter().step_by(23) {
            if d.marginal(&c) {
                continue;
            }
            let model = d.design(&c, rate);
            let (l1, hinf) = (model.l1_norm(), model.hinf_norm());
            if hinf > l1 * (1.0 + 1e-9) {
                return Err(format!("hinf_norm {hinf} above l1_norm {l1}"));
            }
            // A pole of radius `r` has a peak `-ln r * rate / pi` hertz wide. The
            // slack is held where a grid of 2^15 bins (bins `rate / 2^16` apart)
            // resolves that (twice over); the soundness of `hinf_norm` is held on a
            // coarse grid everywhere, since a grid that misses a peak only
            // under-reads it.
            let radius = model.spectral_radius();
            let width =
                -libm::log(radius.max(f64::MIN_POSITIVE)) * rate.get() / std::f64::consts::PI;
            let resolved = radius > 0.0 && width >= 2.0 * rate.get() / f64::from(1u32 << 16);
            let bins: usize = if resolved { 1 << 15 } else { 1 << 12 };
            let freqs: Vec<Hz> = (0..=bins)
                .map(|k| Hz((k as f64 / bins as f64 * rate.get() / 2.0) as f32))
                .collect();
            let mut peak = 0.0f64;
            let mut g = vec![Complex64::new(0.0, 0.0); freqs.len()];
            for output in 0..model.outputs() {
                model.frequency_response(output, &freqs, &mut g);
                peak = g.iter().map(|x| x.norm()).fold(peak, f64::max);
            }
            checked += 1;
            if peak > hinf * (1.0 + 1e-9) {
                return Err(format!(
                    "the response peaks at {peak}, over hinf_norm {hinf} (at {rate:?})"
                ));
            }
            if resolved && hinf > d.hinf_slack() * peak {
                return Err(format!(
                    "hinf_norm {hinf} is more than {} times the peak {peak}",
                    d.hinf_slack()
                ));
            }
        }
        if checked == 0 {
            return Err("no design to measure".into());
        }
        Ok(())
    }

    /// The model's `inputs` and `outputs` are the widths the design declares.
    ///
    /// Mutation (run, in `reverb_design.rs`): a model that counts one output more.
    pub fn ports<D: LtiCase>(d: &D) -> Result<(), String> {
        let channels = d.channels();
        let (i, o) = (channels.input_width(), channels.output_width());
        for c in d.reachable().into_iter().take(8) {
            let m = d.design(&c, d.rates()[0]);
            if (m.inputs(), m.outputs()) != (i, o) {
                return Err(format!(
                    "the model has {} inputs and {} outputs, the design declares {i} and {o}",
                    m.inputs(),
                    m.outputs()
                ));
            }
        }
        Ok(())
    }

    /// The anchors hook returns something, or the design says why it cannot.
    pub fn lti_vacuity<D: LtiCase>(d: &D) -> Result<(), String> {
        check_vacuity(d.vacant(), &[(Hook::Anchors, d.anchors().len())])
    }
}

/// The realization cases of the `Lti` group.
pub(super) mod realization {
    use super::*;

    /// The time from the input's end to each floor (-40, -60 and -90 dB below the
    /// loudest windowed RMS level it reached; a short burst has not
    /// yet filled the lines, so the level after the input stops can exceed the
    /// level at the stop) is within the network's `tail(floor)`: what the node
    /// reports to the graph covers what the realization rings.
    ///
    /// Mutation: the realization retargets every loop gain scaled by 1.03, which rings
    /// 2.4 times as long at the largest room and so past the network's tail.
    pub fn tail_covers<D: LtiCase + RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let mut checked = 0;
        for c in d.probes() {
            if d.marginal(&c) {
                continue;
            }
            let (mut k, network) = built(d, &c, rate);
            let frames = d.fill(rate, 12.0);
            let Ring::Finite(deepest) = network.tail(Db(-90.0)) else {
                continue;
            };
            let after = (deepest.get() * rate.get()) as usize + 4 * WINDOW;
            let b = burst(d.low_tone(rate), frames, after);
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

    /// Sixty seconds of noise at the extreme controls stays finite and under the
    /// l1 norm the model gives (the guarantee against overs: `|x| <= 1` gives
    /// `|y| <= l1_norm`), when the model is stable.
    ///
    /// Mutation: a gain that grows by 0.01% a frame, standing in for a loop gain
    /// above unity (a `LoopGain` mutant would not do: Freeverb's `retarget`
    /// refuses a loop gain of 1 or more, so scaling the network's cannot
    /// destabilise it).
    pub fn stability<D: LtiCase + RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let frames = (60.0 * rate.get()) as usize;
        let mut checked = 0;
        for c in d.extremes() {
            let (mut k, network) = built(d, &c, rate);
            if !network.is_stable() {
                continue;
            }
            let bound = network.l1_norm();
            let out = render(&mut k, widths(d), &noise(3, frames, 1.0), 512);
            let worst = out.iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()));
            checked += 1;
            if out.iter().flatten().any(|s| !s.is_finite()) {
                return Err("a non-finite sample".into());
            }
            if f64::from(worst) > bound {
                return Err(format!("peak {worst} against a bound of {bound}"));
            }
        }
        if checked == 0 {
            return Err("no stable extreme to run".into());
        }
        Ok(())
    }

    /// The realization is linear and shift invariant, at the settled controls of
    /// each static probe (a marginal one is left out): `f(a + b) = f(a) + f(b)`,
    /// `f(3a) = 3 f(a)`, and the input delayed by 1000 frames gives the output
    /// delayed by as many. Each to 1e-4 of the largest output, so a flush of
    /// values under 1e-6 does not count and a nonlinearity does.
    ///
    /// Mutations (run, in `reverb_design.rs`): `Defect::Tanh` (a saturation on the
    /// output) fails the first two; `Defect::PeriodicGain` (a gain that moves with
    /// the frame) is linear and fails the shift.
    pub fn linear<D: LtiCase + RealizationCase>(d: &D) -> Result<(), String> {
        const SHIFT: usize = 1000;
        let rate = rate_of(d);
        let shape = widths(d);
        let (a, b) = (noise(31, 8192, 1.0), noise(32, 8192, 1.0));
        let mut checked = 0;
        for c in d.static_probes() {
            if d.marginal(&c) {
                continue;
            }
            let run = |x: &[f32]| render(&mut built(d, &c, rate).0, shape, x, 64);
            let (fa, fb) = (run(&a), run(&b));
            let sum: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y).collect();
            let triple: Vec<f32> = a.iter().map(|x| 3.0 * x).collect();
            let mut delayed = vec![0.0f32; SHIFT];
            delayed.extend_from_slice(&a[..a.len() - SHIFT]);
            let (fsum, ftriple, fdelayed) = (run(&sum), run(&triple), run(&delayed));
            let scale = fsum
                .iter()
                .flatten()
                .fold(0.0f32, |m, s| m.max(s.abs()))
                .max(1e-9);
            let worst = |pairs: &mut dyn Iterator<Item = (f32, f32)>| {
                pairs.fold(0.0f32, |m, (x, y)| m.max((x - y).abs())) / scale
            };
            let additive = worst(
                &mut fsum
                    .iter()
                    .flatten()
                    .zip(fa.iter().flatten().zip(fb.iter().flatten()))
                    .map(|(s, (x, y))| (*s, x + y)),
            );
            let homogeneous = worst(
                &mut ftriple
                    .iter()
                    .flatten()
                    .zip(fa.iter().flatten())
                    .map(|(t, x)| (*t, 3.0 * x)),
            );
            let invariant = worst(&mut fdelayed.iter().zip(&fa).flat_map(|(late, early)| {
                late[SHIFT..]
                    .iter()
                    .zip(&early[..early.len() - SHIFT])
                    .map(|(l, e)| (*l, *e))
            }));
            checked += 1;
            for (what, err) in [
                ("additivity", additive),
                ("homogeneity", homogeneous),
                ("shift invariance", invariant),
            ] {
                if err > 1e-4 {
                    return Err(format!("{what} fails by {err:.2e} of the largest output"));
                }
            }
        }
        if checked == 0 {
            return Err("no static probe to try".into());
        }
        Ok(())
    }

    /// The realization's impulse response has the model's frequency response:
    /// the `RealFft` of the response (rendered after a retarget to the same
    /// model, which must change nothing) against `frequency_response` on the FFT
    /// bins, the relative error over all of them (`L2`) under
    /// [`response_tolerance`](LtiCase::response_tolerance). Marginal probes and
    /// models with a tail that never ends are left out.
    ///
    /// Mutations (run, in `reverb_design.rs`): `Defect::UnslewedGain(1.1)` (the
    /// output 10% high from the second retarget on) and `Defect::LoopGain(1.05)`
    /// (every comb gain 5% high) fail.
    pub fn response<D: LtiCase + RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let mut checked = 0;
        for c in d.static_probes() {
            if d.marginal(&c) {
                continue;
            }
            let model = d.design(&c, rate);
            let Some(frames) = ir_frames(&model, rate) else {
                continue;
            };
            let (ir, model) = impulse(d, &c, rate, frames);
            let fft = RealFft::new(frames);
            let mut scratch = fft.new_scratch();
            let stride = (fft.bins() / 16_384).max(1);
            let freqs: Vec<Hz> = (0..fft.bins())
                .step_by(stride)
                .map(|k| Hz((k as f64 * rate.get() / frames as f64) as f32))
                .collect();
            for (output, channel) in ir.iter().enumerate().take(model.outputs()) {
                let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
                fft.forward(channel, &mut bins, &mut scratch);
                let mut want = vec![Complex64::new(0.0, 0.0); freqs.len()];
                model.frequency_response(output, &freqs, &mut want);
                let (mut num, mut den) = (0.0f64, 0.0f64);
                for (i, w) in want.iter().enumerate() {
                    let m = bins[i * stride];
                    let got = Complex64::new(f64::from(m.re), f64::from(m.im));
                    num += (got - w).norm_sqr();
                    den += w.norm_sqr();
                }
                let err = (num / den).sqrt();
                checked += 1;
                if err.is_nan() || err > d.response_tolerance() {
                    return Err(format!(
                        "output {output}: the impulse response's spectrum is {err:.3e} from the \
                         model's response (tolerance {})",
                        d.response_tolerance()
                    ));
                }
            }
        }
        if checked == 0 {
            return Err("no probe with an impulse response to measure".into());
        }
        Ok(())
    }

    /// The measured decay per frame is at least the spectral radius's: the energy
    /// of the impulse response in windows of 4096 frames, a least squares line
    /// through the windows between 30 and 80 dB below the loudest, gives a decay
    /// rate that is at least `1 - decay_tolerance` of the rate the radius says
    /// (a radius is a bound: the system may decay faster, never slower). A finite
    /// response (radius 0) has no decay to measure and passes.
    ///
    /// Mutation (run, in `reverb_design.rs`): `Defect::LoopGain(1.05)`, which
    /// decays at 72% of the radius's rate.
    pub fn decay<D: LtiCase + RealizationCase>(d: &D) -> Result<(), String> {
        const W: usize = 4096;
        let rate = rate_of(d);
        let mut checked = 0;
        // Whether every probe is a finite response (radius 0): it has no decay to
        // measure, and the case has nothing to say.
        let mut finite = true;
        for c in d.static_probes() {
            if d.marginal(&c) {
                continue;
            }
            let model = d.design(&c, rate);
            let radius = model.spectral_radius();
            if radius == 0.0 {
                continue;
            }
            finite = false;
            let Some(frames) = ir_frames(&model, rate) else {
                continue;
            };
            if !(radius > 0.0 && radius < 1.0) {
                continue;
            }
            let (ir, _) = impulse(d, &c, rate, frames);
            let energies: Vec<f64> = (0..frames / W)
                .map(|w| super::super::harness::energy(&ir, w * W, W))
                .collect();
            let top = energies.iter().copied().fold(0.0, f64::max);
            let points: Vec<(f64, f64)> = energies
                .iter()
                .enumerate()
                .filter(|(_, e)| **e <= top * 1e-3 && **e >= top * 1e-8)
                .map(|(w, &e)| (w as f64, libm::log(e)))
                .collect();
            if points.len() < 6 {
                continue;
            }
            let n = points.len() as f64;
            let (sx, sy): (f64, f64) = points
                .iter()
                .fold((0.0, 0.0), |(a, b), p| (a + p.0, b + p.1));
            let sxx: f64 = points.iter().map(|p| p.0 * p.0).sum();
            let sxy: f64 = points.iter().map(|p| p.0 * p.1).sum();
            let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
            // `E ~ rho^(2 n)`: the rate per frame is `-slope / (2 W)`.
            let measured = -slope / (2.0 * W as f64);
            let bound = -libm::log(radius);
            checked += 1;
            if measured < (1.0 - d.decay_tolerance()) * bound {
                return Err(format!(
                    "the response decays at {measured:.3e} a frame, the spectral radius {radius} \
                     says at least {bound:.3e}"
                ));
            }
        }
        if checked == 0 && !finite {
            return Err("no probe with a decay to measure".into());
        }
        Ok(())
    }

    /// The tail is tight: the time the impulse response takes to fall to -60 and
    /// -90 dB of its loudest window, measured in windows of [`WINDOW`] frames, is not
    /// less than `1 / tail_slack` of the model's `tail` to that floor (the `tail_covers`
    /// case holds the other direction).
    ///
    /// Mutation (run, in `reverb_design.rs`, on a model whose `tail` is doubled).
    pub fn tail_tight<D: LtiCase + RealizationCase>(d: &D) -> Result<(), String> {
        let rate = rate_of(d);
        let mut checked = 0;
        for c in d.static_probes() {
            if d.marginal(&c) {
                continue;
            }
            let model = d.design(&c, rate);
            let Some(frames) = ir_frames(&model, rate) else {
                continue;
            };
            let (ir, _) = impulse(d, &c, rate, frames);
            let levels: Vec<f64> = (0..=frames - WINDOW)
                .step_by(WINDOW / 4)
                .map(|n| rms(&ir, n, WINDOW))
                .collect();
            let top = levels.iter().copied().fold(0.0, f64::max);
            for floor in [-60.0f32, -90.0] {
                let Ring::Finite(tail) = model.tail(Db(floor)) else {
                    continue;
                };
                let limit = top * f64::from(10f32.powf(floor / 20.0));
                let last = levels.iter().rposition(|&l| l >= limit).unwrap_or(0);
                let took = (last * (WINDOW / 4) + WINDOW / 2) as f64 / rate.get();
                checked += 1;
                if tail.get() > d.tail_slack() * took {
                    return Err(format!(
                        "the tail to {floor} dB is {:.3} s, the response takes {took:.3} s \
                         (slack {})",
                        tail.get(),
                        d.tail_slack()
                    ));
                }
            }
        }
        if checked == 0 {
            return Err("no probe with a finite tail".into());
        }
        Ok(())
    }
}
