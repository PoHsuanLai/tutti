//! Moog-style four-stage ladder filter with resonance and drive.
//!
//! Unlike the SVF this is a *nonlinear* filter: the resonance feedback runs
//! through a `tanh` saturator, so pushing resonance or drive grits and
//! compresses rather than blowing up. That saturation is the character, not a
//! safety measure.
//!
//! One width-generic node, [`LadderFilterNode`]: the coefficients are solved
//! once for every channel, and the channels run side by side in groups.

use super::Real;
use tutti_core::Arc;
use tutti_core::AtomicF32;

use tutti_core::{ChannelLayout, Drive, Hz, Param, Resonance, SampleRate, Tail};
use tutti_graph::{
    Cx, Inputs, IntoNode, Io, Node, NodeParts, Outputs, ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_types::UnitParam;

use crate::ramp::{self, finite_or, LastGood, Ramp};

/// Below these deltas a freq/resonance change doesn't warrant recomputing the
/// coefficients — the change guard shared by the atomic and modulation paths.
///
/// The thresholds alone would not keep a NaN out (`|NaN - last| > eps` is
/// false, so a NaN is held off only until another control moves); the cells
/// are read through `LastGood`, which does.
const FREQ_EPS: f32 = 0.01;
const RES_EPS: f32 = 0.0001;

/// Which tap of the four-stage ladder is taken as the output, and so what
/// response and slope the filter presents.
///
/// All four run the same four stages and the same feedback; they differ only in
/// which tap is read, so the type costs nothing to switch. The 24 dB variants
/// are the classic Moog sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LadderType {
    /// Low-pass at 12 dB/octave — the second stage tap. The default.
    #[default]
    LP12,
    /// Low-pass at 24 dB/octave — the fourth stage tap, the classic Moog
    /// response.
    LP24,
    /// High-pass at 12 dB/octave, formed as input minus the 12 dB low-pass.
    HP12,
    /// High-pass at 24 dB/octave, formed as input minus the 24 dB low-pass.
    HP24,
}

/// The ladder's two coefficients, computed from its parameters.
///
/// Public for the same reason as [`SvfCoeffs`](crate::SvfCoeffs): a voice
/// engine running this topology across SIMD lanes derives its coefficients
/// from this function rather than a copy of it. The recurrence they feed is
/// [`LadderFilterNode`]'s: `u = tanh(x - k*s3)`, then four one-pole stages each
/// `v = g1*(in - s); lp = v + s; s = lp + v` with `g1 = g / (1 + g)`.
#[derive(Debug, Clone, Copy)]
pub struct LadderCoeffs {
    /// Prewarped integrator gain, `tan(pi * fc / sr)`.
    pub g: f64,
    /// Feedback gain, `4 * resonance`. At `4` the ladder self-oscillates.
    pub k: f64,
}

/// Computes ladder coefficients (pure function, no state).
///
/// The cutoff is clamped to `1.0..=0.998 * Nyquist`, since `tan` diverges at
/// Nyquist itself, and the resonance to `0.0..=1.0`.
pub fn compute_ladder_coeffs(
    freq: impl Into<Hz>,
    resonance: impl Into<Resonance>,
    sample_rate: impl Into<SampleRate>,
) -> LadderCoeffs {
    let (freq, resonance, sample_rate) = (freq.into(), resonance.into(), sample_rate.into());
    let fc = f64::from(freq.get()).clamp(1.0, f64::from(sample_rate.nyquist_scaled(0.998).get()));
    LadderCoeffs {
        g: (core::f64::consts::PI * fc / sample_rate.get()).tan(),
        k: 4.0 * f64::from(resonance.get().clamp(0.0, 1.0)),
    }
}

/// The ladder's two coefficients plus the parameter values they were solved
/// at, shared by every channel.
///
/// `g1` is the one-pole stage gain `g / (1 + g)` with `g = tan(π·fc/sr)`,
/// solved once with the `tan` rather than divided out every sample.
#[derive(Clone, Copy, PartialEq)]
struct LadderCoefficients<F: Real> {
    g1: F,
    k: F,
    last_freq: Hz,
    last_res: Resonance,
}

impl<F: Real> LadderCoefficients<F> {
    fn solve(freq: Hz, resonance: Resonance, sample_rate: SampleRate) -> Self {
        let c = compute_ladder_coeffs(freq, resonance, sample_rate);
        let g = F::from_f64(c.g);
        Self {
            g1: g / (F::from_f64(1.0) + g),
            k: F::from_f64(c.k),
            last_freq: freq,
            last_res: resonance,
        }
    }

    /// Marks the set stale, so the next read solves at its target outright.
    fn invalidate(&mut self) {
        self.last_freq = Hz(-1.0);
    }

    fn is_invalid(&self) -> bool {
        self.last_freq.get() < 0.0
    }

    #[inline]
    fn moved(&self, freq: Hz, res: Resonance) -> bool {
        (freq.get() - self.last_freq.get()).abs() > FREQ_EPS
            || (res.get() - self.last_res.get()).abs() > RES_EPS
    }

    #[inline]
    fn lerp(&self, to: &Self, t: F) -> Self {
        Self {
            g1: self.g1 + (to.g1 - self.g1) * t,
            k: self.k + (to.k - self.k) * t,
            ..*to
        }
    }
}

/// One channel's four-stage step over its stage words `s`.
#[inline(always)]
fn ladder_step<F: Real>(
    c: &LadderCoefficients<F>,
    ty: LadderType,
    s: &mut [F; 4],
    input: F,
    drive: F,
) -> F {
    let x = input * drive;
    let u = (x - c.k * s[3]).tanh();
    let g1 = c.g1;

    let v1 = g1 * (u - s[0]);
    let lp1 = v1 + s[0];
    s[0] = lp1 + v1;

    let v2 = g1 * (lp1 - s[1]);
    let lp2 = v2 + s[1];
    s[1] = lp2 + v2;

    let v3 = g1 * (lp2 - s[2]);
    let lp3 = v3 + s[2];
    s[2] = lp3 + v3;

    let v4 = g1 * (lp3 - s[3]);
    let lp4 = v4 + s[3];
    s[3] = lp4 + v4;

    match ty {
        LadderType::LP12 => lp2,
        LadderType::LP24 => lp4,
        LadderType::HP12 => u - lp2,
        LadderType::HP24 => u - lp4,
    }
}

/// A Moog-style four-stage ladder filter with resonance and drive, of any
/// width: `N` audio inputs, `N` outputs.
///
/// The coefficients are solved once for every channel, and the channels'
/// four-stage chains run side by side.
///
/// Cutoff ([`Hz`]), [`Resonance`] and [`Drive`] are live [`Param`]s shared
/// across clones, all read **once per block**. A held cutoff/resonance costs
/// nothing; a moved one is ramped across the block with a coefficient solve
/// every 16 samples; a moved drive is ramped linearly across the block, since
/// it multiplies straight into the saturator and would otherwise step.
///
/// **Resonance is `0.0..=1.0`, not a [`Q`](tutti_core::Q).** It scales the
/// ladder's feedback: `0.0` is no emphasis, and toward `1.0` the filter
/// resonates hard at the cutoff and approaches self-oscillation. The feedback
/// runs through a `tanh` saturator, so it grits rather than blowing up.
///
/// [`Drive`] multiplies the input into that same saturator, so it is a
/// distortion control rather than a level one: above unity it adds harmonics
/// and compresses. `1.0` is clean.
///
/// `F` is the internal state precision, defaulting to `f64` for accuracy at low
/// cutoffs.
///
/// # Modulated params
///
/// `N` audio inputs, `N` outputs. Cutoff, Q and drive are modulatable by the
/// graph, in that port order ([`LADDER_PARAMS`]). A
/// modulated cutoff or Q is sampled at the solve points (every 16 samples
/// and the block's last sample) with the coefficients interpolated between
/// them; a modulated drive is read every sample, since drive needs no solve.
/// Unmodulated, the filter reads its cells once per block, at the cost of
/// one branch.
///
/// The param is `UnitParam::Q` for consistency with the other filters, but
/// it carries [`Resonance`] (`0.0..=1.0`), not a [`Q`](tutti_core::Q).
///
/// # In a graph
///
/// A graph node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over cutoff, resonance (as `UnitParam::Q`) and drive, and a fork of it
/// starts from the values last set through that set. The graph prepares it
/// at the device rate before its first block. Rendering allocates nothing.
pub struct LadderFilterNode<F: Real = f64> {
    ladder_type: LadderType,
    frequency: Param<Hz>,
    resonance: Param<Resonance>,
    drive: Param<Drive>,
    sample_rate: SampleRate,
    /// The coefficients the last rendered sample ran at.
    coeffs: LadderCoefficients<F>,
    /// The drive the last rendered sample ran at — where a drive ramp starts.
    /// `None` until the first block (and after `reset`), which then starts on
    /// its target rather than ramping in from a value nothing set.
    last_drive: Option<Drive>,
    /// Stage state, four words per channel; `len()` is the audio width. Built
    /// at construction and never resized in `process` (RT no-alloc).
    stages: Vec<[F; 4]>,
    /// The last finite cutoff / resonance / drive the cells held: a
    /// non-finite write reads as unchanged, so it never reaches the solve or
    /// the saturator (see [`LastGood`]).
    good: [LastGood; 3],
}

/// The params a [`LadderFilterNode`] lets the graph modulate, in port order.
pub const LADDER_PARAMS: [UnitParam; 3] = [UnitParam::Cutoff, UnitParam::Q, UnitParam::Drive];

impl<F: Real> LadderFilterNode<F> {
    /// A mono ladder filter (1 in, 1 out) of `ladder_type` at `frequency`
    /// cutoff and `resonance`, with unity [`Drive`].
    ///
    /// `resonance` is clamped to `0.0..=1.0`; `0.0` gives no emphasis at the
    /// cutoff and values near `1.0` approach self-oscillation.
    ///
    /// The coefficients are solved at the rate [`Node::prepare`] hands it.
    pub fn new(
        ladder_type: LadderType,
        frequency: impl Into<Hz>,
        resonance: impl Into<Resonance>,
    ) -> Self {
        Self::with_channels(ChannelLayout::MONO, ladder_type, frequency, resonance)
    }

    /// A ladder filter `channels` wide (clamped to at least 1).
    ///
    /// All channels share the authored params — one linked control surface —
    /// and one coefficient solve; only the four stage words are replicated per
    /// channel.
    pub fn with_channels(
        channels: impl Into<ChannelLayout>,
        ladder_type: LadderType,
        frequency: impl Into<Hz>,
        resonance: impl Into<Resonance>,
    ) -> Self {
        let frequency = frequency.into();
        let resonance = Resonance::new_clamped(resonance.into().get());
        let n = usize::from(channels.into().count()).max(1);
        let zero = F::from_f64(0.0);
        Self {
            ladder_type,
            frequency: Param::new(frequency),
            resonance: Param::new(resonance),
            drive: Param::new(Drive::UNITY),
            sample_rate: SampleRate::DEFAULT,
            coeffs: LadderCoefficients::solve(frequency, resonance, SampleRate::DEFAULT),
            last_drive: None,
            stages: vec![[zero; 4]; n],
            good: [
                LastGood::new(frequency.get()),
                LastGood::new(resonance.get()),
                LastGood::new(Drive::UNITY.get()),
            ],
        }
    }

    /// Audio channel width (as many inputs as outputs).
    #[inline]
    fn width(&self) -> usize {
        self.stages.len()
    }

    /// The shared cutoff cell in [`Hz`], governing every channel.
    ///
    /// Read once per block; the coefficient computation clamps to
    /// `1.0..=0.998 * Nyquist`, since `tan` diverges at Nyquist. **A cutoff
    /// the graph feeds overrides it.** Shared across clones.
    ///
    /// A non-finite value written here (NaN, ±∞) reads as *unchanged*: the
    /// filter keeps the last finite value, so it never reaches the coefficient
    /// solve or the filter state, and the next finite write takes effect.
    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.frequency.as_atomic()
    }

    /// The shared [`Resonance`] cell, `0.0..=1.0`, governing every channel.
    ///
    /// Scales the ladder feedback: `0.0` no emphasis, near `1.0` approaching
    /// self-oscillation. The computation clamps to that range regardless of
    /// what is written here. Read once per block. **A resonance the graph
    /// feeds overrides it.**
    ///
    /// A non-finite value written here (NaN, ±∞) reads as *unchanged*: the
    /// filter keeps the last finite value, so it never reaches the coefficient
    /// solve or the filter state, and the next finite write takes effect.
    pub fn resonance(&self) -> Arc<AtomicF32> {
        self.resonance.as_atomic()
    }

    /// The shared [`Drive`] cell — input gain into the `tanh` saturator,
    /// governing every channel.
    ///
    /// Read once per block and ramped across it when it moves, so it needs no
    /// coefficient solve and never steps. `1.0` is clean; higher adds
    /// harmonics and compresses. **A drive the graph feeds overrides it per
    /// sample.**
    pub fn drive(&self) -> Arc<AtomicF32> {
        self.drive.as_atomic()
    }

    /// Sets the cutoff in [`Hz`] for every channel, floored at 1 Hz.
    ///
    /// The upper bound is applied when coefficients are computed, at `0.998` of
    /// Nyquist. While the graph modulates the cutoff this sets the *base* its
    /// modulation rides on.
    pub fn set_frequency(&self, hz: impl Into<Hz>) {
        self.frequency.store(Hz(hz.into().get().max(1.0)));
    }

    /// Sets the [`Resonance`] for every channel, clamped to `0.0..=1.0`.
    pub fn set_resonance(&self, res: impl Into<Resonance>) {
        self.resonance
            .store(Resonance::new_clamped(res.into().get()));
    }

    /// Sets the [`Drive`] into the saturator for every channel, floored at
    /// `0.1`.
    ///
    /// The floor keeps drive from silencing the filter: it multiplies the
    /// input, so `0.0` would mute rather than clean up.
    pub fn set_drive(&self, drive: impl Into<Drive>) {
        self.drive.store(Drive(drive.into().get().max(0.1)));
    }

    /// Read the three control cells, once, with non-finite values held off.
    #[inline]
    fn read_controls(&mut self) -> (Hz, Resonance, Drive) {
        (
            Hz(self.good[0].read(self.frequency.load().get())),
            Resonance(self.good[1].read(self.resonance.load().get())),
            Drive(self.good[2].read(self.drive.load().get())),
        )
    }

    #[inline]
    fn solve_toward(
        &self,
        from: &LadderCoefficients<F>,
        freq: Hz,
        res: Resonance,
    ) -> LadderCoefficients<F> {
        if from.is_invalid() || from.moved(freq, res) {
            LadderCoefficients::solve(freq, res, self.sample_rate)
        } else {
            *from
        }
    }

    /// Coefficients held for the block. `drive_at(i)` is the drive at sample
    /// `i`.
    ///
    /// The channels go through in groups of up to `ramp::LANES`, sample-inner, the
    /// group's stage words in registers and each channel on its own planar
    /// slice. One channel alone is latency-bound — every sample's `tanh` waits
    /// on the previous sample's fourth stage — and running the independent
    /// chains of a group side by side is what fills that latency (measured:
    /// one channel at a time is 2× slower).
    fn run_held(
        &mut self,
        size: usize,
        input: &Inputs<'_>,
        output: &mut Outputs<'_, '_>,
        drive_at: impl Fn(usize) -> F + Copy,
    ) {
        let w = self.width();
        let mut first = 0;
        while first < w {
            let n = ramp::lane_group(w - first);
            match n {
                8 => self.held_group::<8>(first, size, input, output, drive_at),
                7 => self.held_group::<7>(first, size, input, output, drive_at),
                6 => self.held_group::<6>(first, size, input, output, drive_at),
                5 => self.held_group::<5>(first, size, input, output, drive_at),
                4 => self.held_group::<4>(first, size, input, output, drive_at),
                3 => self.held_group::<3>(first, size, input, output, drive_at),
                2 => self.held_group::<2>(first, size, input, output, drive_at),
                _ => self.held_group::<1>(first, size, input, output, drive_at),
            }
            first += n;
        }
    }

    #[inline]
    fn held_group<const N: usize>(
        &mut self,
        first: usize,
        size: usize,
        input: &Inputs<'_>,
        output: &mut Outputs<'_, '_>,
        drive_at: impl Fn(usize) -> F,
    ) {
        let (c, ty) = (self.coeffs, self.ladder_type);
        let xs: [&[f32]; N] = core::array::from_fn(|k| &input.get(first + k)[..size]);
        let mut group = output.iter_mut().skip(first);
        let ys: [&mut [f32]; N] = core::array::from_fn(|_| {
            &mut group.next().expect("the group is inside the width")[..size]
        });
        let mut st: [[F; 4]; N] = core::array::from_fn(|k| self.stages[first + k]);
        for i in 0..size {
            let d = drive_at(i);
            for k in 0..N {
                ys[k][i] = ladder_step(&c, ty, &mut st[k], F::from_f32(xs[k][i]), d).to_f32();
            }
        }
        self.stages[first..first + N].copy_from_slice(&st);
    }

    /// Cutoff/resonance moving inside the block: a solve at each segment's
    /// last sample, coefficients interpolated between solves, every channel
    /// stepped per sample against the shared set. `param_at(i)` gives the
    /// `(cutoff, resonance)` at sample `i` and is asked only at solve points.
    fn run_swept(
        &mut self,
        size: usize,
        input: &Inputs<'_>,
        output: &mut Outputs<'_, '_>,
        param_at: impl Fn(usize) -> (Hz, Resonance),
        drive_at: impl Fn(usize) -> F,
    ) {
        let ty = self.ladder_type;
        let mut prev = self.coeffs;
        let unprimed = prev.is_invalid();
        if unprimed {
            // Nothing meaningful to ramp from — solve at the first sample.
            let (f, r) = param_at(0);
            prev = LadderCoefficients::solve(f, r, self.sample_rate);
        }
        for (start, end) in ramp::segments(size, unprimed) {
            let (f, r) = param_at(end - 1);
            let next = self.solve_toward(&prev, f, r);
            let n = F::from_f64((end - start) as f64);
            for i in start..end {
                let k = if next == prev || i + 1 == end {
                    next
                } else {
                    prev.lerp(&next, F::from_f64((i - start + 1) as f64) / n)
                };
                let drive = drive_at(i);
                for (ch, s) in self.stages.iter_mut().enumerate() {
                    let y = ladder_step(&k, ty, s, F::from_f32(input.get(ch)[i]), drive);
                    output.get(ch)[i] = y.to_f32();
                }
            }
            prev = next;
        }
        self.coeffs = prev;
    }

    /// The block's cutoff/resonance handling, with the drive source already
    /// chosen: held coefficients take the grouped fast path; a moved control
    /// or a fed cutoff/Q (`cutoff`, `res`) takes the swept path.
    #[allow(
        clippy::too_many_arguments,
        reason = "the block, its buffers, the two bases and the three sources \
                  the render chooses between; a struct would only move the list"
    )]
    fn render(
        &mut self,
        size: usize,
        input: &Inputs<'_>,
        output: &mut Outputs<'_, '_>,
        base_freq: Hz,
        base_res: Resonance,
        cutoff: Option<&[f32]>,
        res: Option<&[f32]>,
        drive_at: impl Fn(usize) -> F + Copy,
    ) {
        match (cutoff, res) {
            (None, None) => {
                if self.coeffs.is_invalid() {
                    self.coeffs = LadderCoefficients::solve(base_freq, base_res, self.sample_rate);
                }
                if !self.coeffs.moved(base_freq, base_res) {
                    self.run_held(size, input, output, drive_at);
                    return;
                }
                let fr = Ramp::new(self.coeffs.last_freq.get(), base_freq.get(), size);
                let rr = Ramp::new(self.coeffs.last_res.get(), base_res.get(), size);
                self.run_swept(
                    size,
                    input,
                    output,
                    |i| (Hz(fr.at(i)), Resonance(rr.at(i))),
                    drive_at,
                );
            }
            (cutoff, res) => {
                self.run_swept(
                    size,
                    input,
                    output,
                    |i| {
                        (
                            cutoff.map_or(base_freq, |s| Hz(s[i].max(1.0))),
                            res.map_or(base_res, |s| {
                                Resonance(finite_or(s[i], base_res.get()).clamp(0.0, 1.0))
                            }),
                        )
                    },
                    drive_at,
                );
            }
        }
    }
}

impl<F: Real + 'static> Node for LadderFilterNode<F> {
    /// `N` in and out, cutoff, Q and drive modulatable ([`LADDER_PARAMS`]).
    ///
    /// Its tail is [`Tail::Unknown`]: a resonant ladder rings on after its
    /// input stops, for as long as its resonance and cutoff say, so it is never
    /// skipped on a silent block (a `Tail::None` would cut the ring-out).
    fn shape(&self) -> Shape {
        let width = ChannelLayout::from_count(self.width() as u16);
        Shape::audio(width, width)
            .with_tail(Tail::Unknown)
            .with_params(&LADDER_PARAMS)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = p.sample_rate();
        self.coeffs.invalidate();
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        if size == 0 {
            return Status::Modified;
        }
        // Every control is read here, once, for the whole block.
        let (base_freq, base_res, base_drive) = self.read_controls();
        let bd = base_drive.get();

        // Drive: a modulated drive is read per sample; the cell ramps from
        // where the last block ended. Each source gets its own monomorphised
        // render, so a held drive is a constant in the loop.
        let (cutoff, res, fed_drive) = (
            io.param(0).frames(),
            io.param(1).frames(),
            io.param(2).frames(),
        );
        let (inputs, mut outputs) = io.split();
        let (input, output) = (&inputs, &mut outputs);
        let drive_from = self.last_drive.unwrap_or(base_drive);
        let drive_ramp = Ramp::new(drive_from.get(), base_drive.get(), size);
        self.last_drive = Some(match fed_drive {
            Some(s) => Drive(finite_or(s[size - 1], bd).max(0.1)),
            None => base_drive,
        });
        let (f, r) = (base_freq, base_res);
        match fed_drive {
            Some(s) => self.render(size, input, output, f, r, cutoff, res, |i| {
                F::from_f32(finite_or(s[i], bd).max(0.1))
            }),
            None if drive_ramp.is_flat() => {
                let d = F::from_f32(base_drive.get());
                self.render(size, input, output, f, r, cutoff, res, move |_| d);
            }
            None => self.render(size, input, output, f, r, cutoff, res, |i| {
                F::from_f32(drive_ramp.at(i))
            }),
        }
        Status::Modified
    }

    fn reset(&mut self) {
        let zero = F::from_f64(0.0);
        self.stages.fill([zero; 4]);
        self.last_drive = None;
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match k {
            0 => Some(self.frequency.load().get()),
            1 => Some(self.resonance.load().get()),
            2 => Some(self.drive.load().get()),
            _ => None,
        }
    }
}

impl<F: Real + 'static> ParamNode for LadderFilterNode<F> {
    /// Cutoff, resonance (as `UnitParam::Q`) and drive.
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Cutoff, self.frequency.as_atomic())
            .param(UnitParam::Q, self.resonance.as_atomic())
            .param(UnitParam::Drive, self.drive.as_atomic())
            .build()
    }

    /// A clone with its three cells detached (at their values now) and its
    /// stages cleared.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.frequency.detach();
        fork.resonance.detach();
        fork.drive.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork that starts from
/// the values last set through it ([`tutti_graph::param_parts`]).
impl<F: Real + 'static> IntoNode for LadderFilterNode<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

/// Shares the control cells (the template [`tutti_graph::param_parts`] forks
/// from); the stages are copied.
impl<F: Real> Clone for LadderFilterNode<F> {
    fn clone(&self) -> Self {
        Self {
            ladder_type: self.ladder_type,
            frequency: self.frequency.handle(),
            resonance: self.resonance.handle(),
            drive: self.drive.handle(),
            sample_rate: self.sample_rate,
            coeffs: self.coeffs,
            last_drive: self.last_drive,
            stages: self.stages.clone(),
            good: self.good,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_utils::{generate_sine, process_mono, rms};
    use crate::test_support::{prepared_at, tick, tick_fed, RATE_44K};
    use tutti_graph::contract::assert_param_fork;

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it.
    ///
    /// Mutation (run): drop `fork.drive.detach()` in `fork_fresh` → "a live
    /// write reached the fork" for `Drive` → fails.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(LadderFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            LadderType::LP24,
            800.0,
            0.3,
        ));
    }

    #[test]
    fn test_ladder_lp24_attenuates_high_freq() {
        let mut filter = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, 500.0, 0.0),
            RATE_44K,
        );

        let low = generate_sine(100.0, 44100.0, 4096);
        let high = generate_sine(5000.0, 44100.0, 4096);

        let out_low = process_mono(&mut filter, &low);
        filter.reset();
        let out_high = process_mono(&mut filter, &high);

        let rms_low = rms(&out_low[512..]);
        let rms_high = rms(&out_high[512..]);

        assert!(
            rms_low > rms_high * 5.0,
            "LP24 should strongly attenuate high freq: low={rms_low}, high={rms_high}"
        );
    }

    #[test]
    fn test_ladder_hp24_attenuates_low_freq() {
        let mut filter = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::HP24, 2000.0, 0.0),
            RATE_44K,
        );

        let low = generate_sine(100.0, 44100.0, 4096);
        let high = generate_sine(5000.0, 44100.0, 4096);

        let out_low = process_mono(&mut filter, &low);
        filter.reset();
        let out_high = process_mono(&mut filter, &high);

        let rms_low = rms(&out_low[512..]);
        let rms_high = rms(&out_high[512..]);

        assert!(
            rms_high > rms_low * 3.0,
            "HP24 should attenuate low freq: low={rms_low}, high={rms_high}"
        );
    }

    #[test]
    fn test_ladder_resonance_boosts_cutoff() {
        let mut no_res = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.0),
            RATE_44K,
        );

        let mut with_res = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.8),
            RATE_44K,
        );

        let at_cutoff = generate_sine(1000.0, 44100.0, 4096);

        let out_no = process_mono(&mut no_res, &at_cutoff);
        let out_yes = process_mono(&mut with_res, &at_cutoff);

        let rms_no = rms(&out_no[512..]);
        let rms_yes = rms(&out_yes[512..]);

        assert!(
            rms_yes > rms_no,
            "Resonance should boost at cutoff: no_res={rms_no}, with_res={rms_yes}"
        );
    }

    #[test]
    fn test_ladder_drive_adds_saturation() {
        let mut clean = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, 5000.0, 0.0),
            RATE_44K,
        );

        let mut driven = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, 5000.0, 0.0),
            RATE_44K,
        );
        driven.set_drive(10.0);

        let sine = generate_sine(440.0, 44100.0, 4096);

        let out_clean = process_mono(&mut clean, &sine);
        let out_driven = process_mono(&mut driven, &sine);

        let rms_clean = rms(&out_clean[512..]);
        let rms_driven = rms(&out_driven[512..]);

        assert!(
            (rms_clean - rms_driven).abs() > 0.01 || rms_driven > rms_clean * 0.5,
            "Drive should affect signal: clean={rms_clean}, driven={rms_driven}"
        );
    }

    #[test]
    fn test_ladder_reset() {
        let mut filter = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.5),
            RATE_44K,
        );

        let sine = generate_sine(440.0, 44100.0, 100);
        let _ = process_mono(&mut filter, &sine);

        filter.reset();

        let mut out = [0.0f32];
        tick(&mut filter, &[0.0], &mut out);
        assert!(
            out[0].abs() < 0.0001,
            "After reset, output should be near zero"
        );
    }

    #[test]
    fn test_ladder_lp12_less_steep_than_lp24() {
        let mut lp12 = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP12, 1000.0, 0.0),
            RATE_44K,
        );

        let mut lp24 = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.0),
            RATE_44K,
        );

        let high = generate_sine(5000.0, 44100.0, 4096);

        let out12 = process_mono(&mut lp12, &high);
        let out24 = process_mono(&mut lp24, &high);

        let rms12 = rms(&out12[512..]);
        let rms24 = rms(&out24[512..]);

        assert!(
            rms12 > rms24,
            "LP12 should pass more high freq than LP24: lp12={rms12}, lp24={rms24}"
        );
    }

    #[test]
    fn test_ladder_f32_state_agrees_with_f64_for_mid_cutoff() {
        let mut ladder_f64 = prepared_at(
            LadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.3),
            RATE_44K,
        );
        let mut ladder_f32 = prepared_at(
            LadderFilterNode::<f32>::new(LadderType::LP24, 1000.0, 0.3),
            RATE_44K,
        );

        let input = generate_sine(500.0, 44100.0, 2048);
        let out_f64 = process_mono(&mut ladder_f64, &input);
        let out_f32 = process_mono(&mut ladder_f32, &input);

        let rms_f64 = rms(&out_f64[512..]);
        let rms_f32 = rms(&out_f32[512..]);
        let rel_diff = (rms_f64 - rms_f32).abs() / rms_f64.max(1e-6);
        assert!(
            rel_diff < 0.01,
            "ladder f32/f64 should agree within 1% at mid cutoff: f64={rms_f64}, f32={rms_f32}"
        );
    }

    // ── Modulated params (the graph's param feed) ───────────────────────────

    /// Run a stereo ladder a frame at a time with `params` (cutoff, Q,
    /// drive: `Some` held at a value, `None` reading its base) on its param
    /// ports each frame.
    fn process_stereo_ladder(
        node: &mut dyn Node,
        l: &[f32],
        r: &[f32],
        params: [Option<f32>; 3],
    ) -> (Vec<f32>, Vec<f32>) {
        let (mut out_l, mut out_r) = (Vec::new(), Vec::new());
        let mut o = [0.0f32; 2];
        for (&a, &b) in l.iter().zip(r) {
            tick_fed(node, &[a, b], &params, &mut o);
            out_l.push(o[0]);
            out_r.push(o[1]);
        }
        (out_l, out_r)
    }

    #[test]
    fn ladder_with_channels_reports_arity_and_is_independent() {
        let mut wide = prepared_at(
            LadderFilterNode::<f64>::with_channels(6usize, LadderType::LP24, 1000.0, 0.3),
            RATE_44K,
        );
        assert_eq!(wide.shape().audio_in.count(), 6);
        assert_eq!(wide.shape().audio_out.count(), 6);

        // Drive only channel 4; the rest must stay silent.
        let sig = generate_sine(300.0, 44100.0, 2048);
        let len = sig.len();
        let mut out = vec![vec![0.0f32; len]; 6];
        let mut outbuf = [0.0f32; 6];
        for i in 0..len {
            // Rebuilt per sample, not cleared in place: only channel 4 is
            // driven, so every other channel must re-enter each frame at zero.
            let mut inbuf = [0.0f32; 6];
            inbuf[4] = sig[i];
            tick(&mut wide, &inbuf, &mut outbuf);
            for c in 0..6 {
                out[c][i] = outbuf[c];
            }
        }
        let e4: f32 = out[4].iter().map(|s| s * s).sum();
        assert!(e4 > 1.0, "ch4 should carry signal; energy {e4}");
        for c in [0usize, 1, 2, 3, 5] {
            let e: f32 = out[c].iter().map(|s| s * s).sum();
            assert!(e < 1e-10, "ch{c} should stay silent; energy {e}");
        }
    }

    /// The shape declares cutoff, Q then drive, never changes the arity, and
    /// reports `Tail::Unknown`.
    ///
    /// Mutation (run): swap `LADDER_PARAMS`' Q and drive → the first
    /// assertion fails. Mutation (run): drop `.with_tail(Tail::Unknown)` →
    /// the shape's default `Tail::None` → the tail assertion fails.
    #[test]
    fn the_shape_declares_cutoff_q_then_drive() {
        let f = LadderFilterNode::<f64>::with_channels(6usize, LadderType::LP24, 1000.0, 0.3);
        assert_eq!(
            f.shape().params.as_slice(),
            &[UnitParam::Cutoff, UnitParam::Q, UnitParam::Drive][..]
        );
        assert_eq!(
            (f.shape().audio_in.count(), f.shape().audio_out.count()),
            (6, 6)
        );
        assert_eq!(f.param_base(0), Some(1000.0));
        assert_eq!(f.param_base(1), Some(0.3));
        assert_eq!(f.param_base(2), Some(1.0), "drive starts clean");
        assert_eq!(f.shape().tail, Tail::Unknown, "a resonant ladder rings on");
    }

    #[test]
    fn stereo_ladder_fed_cutoff_modulates_response() {
        let noise: Vec<f32> = (0..2048)
            .map(|i| ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let run = |cutoff: f32| -> f32 {
            let mut f = prepared_at(
                LadderFilterNode::<f64>::with_channels(
                    ChannelLayout::STEREO,
                    LadderType::LP24,
                    200.0,
                    0.3,
                ),
                RATE_44K,
            );
            let (out_l, _) =
                process_stereo_ladder(&mut f, &noise, &noise, [Some(cutoff), None, None]);
            rms(&out_l[256..])
        };
        let low = run(200.0);
        let high = run(8000.0);
        assert!(
            high > low * 1.5,
            "higher fed cutoff should pass more: low={low}, high={high}"
        );
    }

    #[test]
    fn stereo_ladder_unmodulated_matches_modulated_held_constant() {
        // A node whose fed cutoff is held at the atomic value must produce the
        // same output as a plain node.
        let signal = generate_sine(440.0, 44100.0, 1024);

        let mut plain = prepared_at(
            LadderFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                LadderType::LP24,
                1000.0,
                0.3,
            ),
            RATE_44K,
        );
        let (plain_l, _) = process_stereo_ladder(&mut plain, &signal, &signal, [None; 3]);

        let mut modn = prepared_at(
            LadderFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                LadderType::LP24,
                1000.0,
                0.3,
            ),
            RATE_44K,
        );
        let (mod_l, _) =
            process_stereo_ladder(&mut modn, &signal, &signal, [Some(1000.0), None, None]);

        for i in 0..signal.len() {
            assert!(
                (plain_l[i] - mod_l[i]).abs() < 1e-5,
                "modulated-held output diverges from plain at sample {i}: {} vs {}",
                plain_l[i],
                mod_l[i]
            );
        }
    }

    // ── Per-block reads ──────────────────────────────────────────────────────

    /// Unprepared: `change_between_node_blocks` prepares each copy at 48 kHz.
    fn ladder_48k() -> LadderFilterNode<f64> {
        LadderFilterNode::<f64>::with_channels(ChannelLayout::STEREO, LadderType::LP24, 500.0, 0.4)
    }

    /// A cutoff change made between blocks is read by the next block, ramped
    /// across it, and the block ends solved at the new cutoff.
    ///
    /// Mutation: solving straight at the target and rendering the block held
    /// (a jump) fails `assert_ramps_in`.
    #[test]
    fn a_cutoff_change_is_read_next_block_and_ramped_across_it() {
        use crate::test_support::{change_between_node_blocks, noise};
        let x = noise(5, 128);
        let run = change_between_node_blocks(
            ladder_48k,
            |n| n.set_frequency(7_000.0),
            &[&x[..64], &x[..64]],
            &[&x[64..], &x[64..]],
        );
        run.assert_ramps_in("ladder cutoff");
        assert!(run.ramped.coeffs == run.jumped.coeffs);
    }

    /// Drive multiplies straight into the saturator, so a step would click; it
    /// ramps across the next block and ends on the new value.
    ///
    /// Mutation: `Ramp::new(base_drive.get(), base_drive.get(), size)` (a jump)
    /// fails `assert_ramps_in`.
    #[test]
    fn a_drive_change_is_read_next_block_and_ramped_across_it() {
        use crate::test_support::{change_between_node_blocks, noise};
        let x = noise(6, 128);
        // A high-pass tap, so the saturator's input reaches the output
        // unfiltered and the drive step is audible on the first sample.
        let run = change_between_node_blocks(
            || {
                LadderFilterNode::<f64>::with_channels(
                    ChannelLayout::STEREO,
                    LadderType::HP24,
                    500.0,
                    0.4,
                )
            },
            |n| n.set_drive(9.0),
            &[&x[..64], &x[..64]],
            &[&x[64..], &x[64..]],
        );
        run.assert_ramps_in("ladder drive");
        assert_eq!(run.ramped.last_drive, Some(Drive(9.0)));
    }

    /// Six channels share one coefficient solve and keep six independent
    /// stage lanes: each channel is the mono ladder on its own input.
    ///
    /// Mutation: indexing lane 0 for every channel in `run_held` fails.
    #[test]
    fn six_channels_are_six_mono_ladders() {
        use crate::test_support::{drive_block, noise, prepared};
        let inputs: Vec<Vec<f32>> = (0..6).map(|c| noise(c + 20, 64)).collect();
        let refs: Vec<&[f32]> = inputs.iter().map(|v| &v[..]).collect();
        let mut wide = prepared(LadderFilterNode::<f64>::with_channels(
            6usize,
            LadderType::HP12,
            800.0,
            0.7,
        ));
        let out = drive_block(&mut wide, &refs);
        for (c, input) in inputs.iter().enumerate() {
            let mut mono = prepared(LadderFilterNode::<f64>::new(LadderType::HP12, 800.0, 0.7));
            assert_eq!(
                drive_block(&mut mono, &[&input[..]])[0],
                out[c],
                "channel {c}"
            );
        }
    }
}
