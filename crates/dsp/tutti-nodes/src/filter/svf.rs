//! State-variable filter — the crate's general-purpose filter.
//!
//! One coefficient computation serves eight responses ([`SvfType`]), so
//! switching type is as cheap as any parameter change. One width-generic node,
//! [`SvfFilterNode`]: the coefficients are shared across channels and only the
//! integrator state is per channel.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_dsp::Real;

use tutti_core::{ChannelLayout, Db, Hz, Param, Q, SampleRate, Tail};
use tutti_graph::{
    Cx, Inputs, IntoNode, Io, Node, NodeParts, Outputs, ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_types::UnitParam;

use crate::ramp::{self, LastGood, Ramp};

use tutti_dsp::{SvfCoefficients, SvfType, svf_step};

/// A state-variable filter of any width: `N` audio inputs, `N` outputs, one
/// coefficient set shared across the channels and one integrator pair per
/// channel.
///
/// Eight responses ([`SvfType`]) share one coefficient solve (Cytomic /
/// Zavalishin TPT form), so switching type is as cheap as any parameter
/// change.
///
/// # Examples
///
/// ```
/// use tutti_core::{Hz, Q, UnitParam};
/// use tutti_graph::{Prepare, Solo};
/// use tutti_nodes::{SvfFilterNode, SvfType};
/// use tutti_types::{SampleRate, Samples};
///
/// let filter = SvfFilterNode::<f32>::new(SvfType::LowPass, Hz(800.0), Q(0.707));
/// let mut solo = Solo::new(filter, Prepare::new(SampleRate(48_000.0), Samples(64)));
/// let out = solo.render_input(&[&[1.0; 64]]);
/// assert_eq!(out.len(), 1);
///
/// // Move the cutoff on the running node; it glides over the next block.
/// assert!(solo.controls().set(UnitParam::Cutoff, 2_000.0));
/// ```
///
/// Cutoff ([`Hz`]), [`Q`] and gain ([`Db`]) are live [`Param`]s shared across
/// clones, read **once per block**. A held value costs nothing — the
/// coefficients are solved only when one moves past a small epsilon. A value
/// that *did* move is ramped across the block: the coefficients are re-solved
/// every 16 samples along a linear parameter ramp and interpolated between
/// solves, so an automated cutoff glides rather than stepping at block edges.
///
/// Gain applies only to [`Bell`](SvfType::Bell),
/// [`LowShelf`](SvfType::LowShelf) and [`HighShelf`](SvfType::HighShelf); the
/// other types ignore it.
///
/// `F` is the internal state precision. It defaults to `f64` for accuracy at
/// low cutoffs, where an `f32` integrator loses resolution in the feedback
/// path; `SvfFilterNode::<f32>::new(…)` trades that for CPU.
///
/// # Modulated params
///
/// `N` audio inputs, `N` outputs. Cutoff (in [`Hz`]), [`Q`] and the gain
/// (in [`Db`]) are modulatable by the graph, in that port order
/// ([`SVF_PARAMS`]). The gain is read once per block, as its cell is. For
/// cutoff and Q, a per-frame value on the param port
/// ([`Io::param`](tutti_graph::Io::param)) **overrides** the corresponding
/// cell. It is sampled at the solve points — every 16 samples and at the
/// block's last sample — with the coefficients interpolated between them,
/// instead of a `tan` per sample. Unmodulated, the filter reads its cells
/// once per block at the cost of one branch, which is the common case.
///
/// # In a graph
///
/// A graph node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over cutoff, Q and gain by [`UnitParam`], and a fork of it starts from
/// the values last set through that set. The graph prepares it at the
/// device rate before its first block, so it is never run at the placeholder
/// rate it is built at. Rendering allocates nothing.
pub struct SvfFilterNode<F: Real = f64> {
    filter_type: SvfType,
    frequency: Param<Hz>,
    q: Param<Q>,
    gain_db: Param<Db>,
    sample_rate: SampleRate,
    /// The coefficients the last rendered sample ran at.
    coeffs: SvfCoefficients<F>,
    /// Integrator state `[ic1eq, ic2eq]`, one pair per channel; `len()` is the
    /// audio width. Built at construction — never resized in `process`
    /// (RT no-alloc).
    state: Vec<[F; 2]>,
    /// The last finite cutoff / Q / gain the cells held: a non-finite write
    /// reads as unchanged, so it never reaches the solve (see [`LastGood`]).
    good: [LastGood; 3],
}

impl<F: Real> std::fmt::Debug for SvfFilterNode<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SvfFilterNode")
            .field("filter_type", &self.filter_type)
            .field("frequency", &self.frequency)
            .field("q", &self.q)
            .field("gain_db", &self.gain_db)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

/// The params an [`SvfFilterNode`] lets the graph modulate, in port order.
/// Cutoff and Q are read per sample; the gain (a shelf's or a bell's) is
/// block-rate in the node (ramped across the block), so a driven port is
/// read once per block (`block_driven`).
pub const SVF_PARAMS: [UnitParam; 3] = [UnitParam::Cutoff, UnitParam::Q, UnitParam::GainDb];

impl<F: Real> SvfFilterNode<F> {
    /// A mono filter (1 in, 1 out) of `filter_type` at `frequency` cutoff and
    /// resonance `q`, with 0 dB gain.
    ///
    /// `q` around `0.707` is the flattest (Butterworth) response; higher values
    /// resonate at the cutoff, and a band-pass or notch narrows as it rises.
    ///
    /// The coefficients are solved at the rate [`Node::prepare`] hands it.
    pub fn new(filter_type: SvfType, frequency: impl Into<Hz>, q: impl Into<Q>) -> Self {
        Self::with_channels(ChannelLayout::MONO, filter_type, frequency, q)
    }

    /// A filter `channels` wide (clamped to at least 1).
    ///
    /// The coefficients are channel-shared — one linked control surface across
    /// all channels — and only the per-channel integrator state is replicated,
    /// so widening costs state but not parameter work.
    ///
    /// Speaker placement is the upstream panner's job: this is a per-channel
    /// filter, not a spatial process.
    ///
    pub fn with_channels(
        channels: impl Into<ChannelLayout>,
        filter_type: SvfType,
        frequency: impl Into<Hz>,
        q: impl Into<Q>,
    ) -> Self {
        let frequency = frequency.into();
        let q = q.into();
        let n = usize::from(channels.into().count()).max(1);
        let zero = F::from_f64(0.0);
        Self {
            filter_type,
            frequency: Param::new(frequency),
            q: Param::new(q),
            gain_db: Param::new(Db(0.0)),
            sample_rate: SampleRate::DEFAULT,
            coeffs: SvfCoefficients::solve(filter_type, frequency, q, Db(0.0), SampleRate::DEFAULT),
            state: vec![[zero; 2]; n],
            good: [
                LastGood::new(frequency.get()),
                LastGood::new(q.get()),
                LastGood::new(0.0),
            ],
        }
    }

    /// Audio channel width (`inputs()` audio ports == `outputs()`).
    #[inline]
    fn width(&self) -> usize {
        self.state.len()
    }

    /// Sets the shelf/bell gain in [`Db`], recomputing coefficients
    /// immediately. Read only by the gain-bearing filter types.
    #[must_use]
    pub fn with_gain_db(mut self, db: impl Into<Db>) -> Self {
        let db = db.into();
        self.gain_db = Param::new(db);
        self.good[2] = LastGood::new(db.get());
        self.coeffs = SvfCoefficients::solve(
            self.filter_type,
            self.frequency.load(),
            self.q.load(),
            db,
            self.sample_rate,
        );
        self
    }

    /// The shared cutoff cell in [`Hz`], applied to every channel alike.
    ///
    /// Read once per block. Writing the raw cell bypasses the 1 Hz floor
    /// [`set_frequency`](Self::set_frequency) applies; the coefficient
    /// computation clamps to `1.0..=0.998 * Nyquist` regardless, since `tan`
    /// diverges at Nyquist. **A cutoff the graph feeds overrides it.**
    /// Shared across clones.
    ///
    /// A non-finite value written here (NaN, ±∞) reads as *unchanged*: the
    /// filter keeps the last finite value, so it never reaches the coefficient
    /// solve or the filter state, and the next finite write takes effect.
    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.frequency.as_atomic()
    }

    /// The shared resonance cell in [`Q`], applied to every channel alike.
    ///
    /// Read once per block. Writing the raw cell bypasses
    /// [`set_q`](Self::set_q)'s clamp; the coefficient computation floors the
    /// value at `0.01` to avoid a division blow-up. **A Q the graph feeds
    /// overrides it.** Shared across clones.
    ///
    /// A non-finite value written here (NaN, ±∞) reads as *unchanged*: the
    /// filter keeps the last finite value, so it never reaches the coefficient
    /// solve or the filter state, and the next finite write takes effect.
    pub fn q(&self) -> Arc<AtomicF32> {
        self.q.as_atomic()
    }

    /// The shared shelf/bell gain cell in [`Db`].
    ///
    /// Always read from the atomic — the graph cannot modulate it. Inert
    /// on the non-gain filter types.
    ///
    /// A non-finite value written here (NaN, ±∞) reads as *unchanged*: the
    /// filter keeps the last finite value, so it never reaches the coefficient
    /// solve or the filter state, and the next finite write takes effect.
    pub fn gain_db(&self) -> Arc<AtomicF32> {
        self.gain_db.as_atomic()
    }

    /// Sets the cutoff in [`Hz`] for every channel, floored at 1 Hz.
    ///
    /// The upper bound is applied when coefficients are computed, at `0.998` of
    /// Nyquist — so this accepts a higher value and the filter tops out there.
    /// While the graph modulates the cutoff this sets the *base* its
    /// modulation rides on, not what the filter runs at.
    pub fn set_frequency(&self, hz: impl Into<Hz>) {
        self.frequency.store(Hz(hz.into().get().max(1.0)));
    }

    /// Sets the resonance [`Q`] for every channel, clamped to the valid range.
    ///
    /// `0.707` is the flattest response; higher resonates at the cutoff.
    pub fn set_q(&self, q: impl Into<Q>) {
        self.q.store(Q::new_clamped(q.into().get()));
    }

    /// Sets the shelf/bell gain in [`Db`]. Inert on the non-gain filter types.
    pub fn set_gain_db(&self, db: impl Into<Db>) {
        self.gain_db.store(db.into());
    }

    /// Switches the filter response for every channel, forcing a coefficient
    /// solve on the next sample.
    ///
    /// `&mut self`, so this cannot reach a node already live in the graph —
    /// the response is a structural choice, not an automatable parameter.
    /// Integrator state is deliberately kept, so the switch is continuous
    /// rather than a click.
    pub fn set_filter_type(&mut self, filter_type: SvfType) {
        self.filter_type = filter_type;
        self.coeffs.invalidate();
    }

    /// Read the three control cells, once, with non-finite values held off.
    /// `gain_driven` is the block's gain from the node's param port when a
    /// lane drives it; `None` reads the cell.
    #[inline]
    fn read_controls(&mut self, gain_driven: Option<f32>) -> (Hz, Q, Db) {
        (
            Hz(self.good[0].read(self.frequency.load().get())),
            Q(self.good[1].read(self.q.load().get())),
            Db(self.good[2].read(gain_driven.unwrap_or_else(|| self.gain_db.load().get()))),
        )
    }

    /// Solve at `(freq, q, gain_db)` unless the current set is already there.
    #[inline]
    fn solve_toward(
        &self,
        from: &SvfCoefficients<F>,
        freq: Hz,
        q: Q,
        gain_db: Db,
    ) -> SvfCoefficients<F> {
        if from.is_invalid() || from.moved(freq, q, gain_db) {
            SvfCoefficients::solve(self.filter_type, freq, q, gain_db, self.sample_rate)
        } else {
            *from
        }
    }

    /// Render with the current coefficients held for the whole block.
    ///
    /// The channels go through in groups of up to `ramp::LANES`, sample-inner, with
    /// the group's integrators in registers. A recursive filter is
    /// latency-bound on one channel — each sample waits for the last — so
    /// running one channel at a time (measured: 2.7× slower at width 6) wastes
    /// the independent chains the other channels offer; a group interleaves
    /// them while still reading and writing planar slices.
    fn run_held(&mut self, size: usize, input: &Inputs<'_>, output: &mut Outputs<'_, '_>) {
        let w = self.width();
        let mut first = 0;
        while first < w {
            let n = ramp::lane_group(w - first);
            match n {
                8 => self.held_group::<8>(first, size, input, output),
                7 => self.held_group::<7>(first, size, input, output),
                6 => self.held_group::<6>(first, size, input, output),
                5 => self.held_group::<5>(first, size, input, output),
                4 => self.held_group::<4>(first, size, input, output),
                3 => self.held_group::<3>(first, size, input, output),
                2 => self.held_group::<2>(first, size, input, output),
                _ => self.held_group::<1>(first, size, input, output),
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
    ) {
        let c = self.coeffs;
        let xs: [&[f32]; N] = core::array::from_fn(|k| &input.get(first + k)[..size]);
        let mut group = output.iter_mut().skip(first);
        let ys: [&mut [f32]; N] = core::array::from_fn(|_| {
            &mut group.next().expect("the group is inside the width")[..size]
        });
        let mut st: [[F; 2]; N] = core::array::from_fn(|k| self.state[first + k]);
        for i in 0..size {
            for k in 0..N {
                ys[k][i] = svf_step(&c, &mut st[k], F::from_f32(xs[k][i])).to_f32();
            }
        }
        self.state[first..first + N].copy_from_slice(&st);
    }

    /// Render with the parameters moving inside the block. `param_at(i)` is the
    /// `(cutoff, Q, gain)` at sample `i`; it is only asked at the solve points —
    /// each segment's last sample — and the coefficients are interpolated
    /// linearly between consecutive solves. The last sample of each segment runs
    /// at its solved set exactly, so a segment of one sample (a block of one) is the
    /// per-sample solve it replaces.
    fn run_swept(
        &mut self,
        size: usize,
        input: &Inputs<'_>,
        output: &mut Outputs<'_, '_>,
        param_at: impl Fn(usize) -> (Hz, Q, Db),
    ) {
        let width = self.width();
        let mut prev = self.coeffs;
        let unprimed = prev.is_invalid();
        if unprimed {
            // Nothing meaningful to ramp from — solve at the first sample.
            let (f, q, g) = param_at(0);
            prev = SvfCoefficients::solve(self.filter_type, f, q, g, self.sample_rate);
        }
        for (start, end) in ramp::segments(size, unprimed) {
            let (f, q, g) = param_at(end - 1);
            let next = self.solve_toward(&prev, f, q, g);
            let n = F::from_f64((end - start) as f64);
            for i in start..end {
                let k = if next == prev || i + 1 == end {
                    next
                } else {
                    prev.lerp(&next, F::from_f64((i - start + 1) as f64) / n)
                };
                for ch in 0..width {
                    let x = F::from_f32(input.get(ch)[i]);
                    let y = svf_step(&k, &mut self.state[ch], x);
                    output.get(ch)[i] = y.to_f32();
                }
            }
            prev = next;
        }
        self.coeffs = prev;
    }
}

impl<F: Real + 'static> Node for SvfFilterNode<F> {
    /// Its tail is [`Tail::Unknown`]: a resonant filter rings on after its
    /// input stops, for as long as its Q says, so the graph's tail fold (what
    /// an export renders past the end) must not read it as `None`.
    fn shape(&self) -> Shape {
        let width = ChannelLayout::from_count(self.width() as u16);
        Shape::audio(width, width)
            .with_cost(tutti_graph::Cost::Moderate)
            .with_params(&SVF_PARAMS)
            .with_tail(Tail::Unknown)
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
        // Every control is read here, once, for the whole block. A modulated
        // value's `max` floor maps a NaN sample to the floor, and ±∞ is
        // clamped by the solve, so only the cells need holding off.
        let (base_freq, base_q, gain_db) = self.read_controls(crate::ramp::block_driven(&io, 2));
        let (cutoff, res) = (io.param(0).frames(), io.param(1).frames());
        let (inputs, mut outputs) = io.split();
        if cutoff.is_none() && res.is_none() {
            if self.coeffs.is_invalid() {
                // A new rate or response type: solve at the target outright.
                self.coeffs = SvfCoefficients::solve(
                    self.filter_type,
                    base_freq,
                    base_q,
                    gain_db,
                    self.sample_rate,
                );
            }
            if !self.coeffs.moved(base_freq, base_q, gain_db) {
                self.run_held(size, &inputs, &mut outputs);
                return Status::Modified;
            }
            // Moved since the last block: glide there across this one.
            let fr = Ramp::new(self.coeffs.last_freq.get(), base_freq.get(), size);
            let qr = Ramp::new(self.coeffs.last_q.get(), base_q.get(), size);
            let gr = Ramp::new(self.coeffs.last_gain_db.get(), gain_db.get(), size);
            self.run_swept(size, &inputs, &mut outputs, |i| {
                (Hz(fr.at(i)), Q(qr.at(i)), Db(gr.at(i)))
            });
        } else {
            self.run_swept(size, &inputs, &mut outputs, |i| {
                (
                    cutoff.map_or(base_freq, |s| Hz(s[i].max(1.0))),
                    res.map_or(base_q, |s| Q(s[i].max(0.01))),
                    gain_db,
                )
            });
        }
        Status::Modified
    }

    fn reset(&mut self) {
        let zero = F::from_f64(0.0);
        self.state.fill([zero; 2]);
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match k {
            0 => Some(self.frequency.load().get()),
            1 => Some(self.q.load().get()),
            2 => Some(self.gain_db.load().get()),
            _ => None,
        }
    }
}

impl<F: Real + 'static> ParamNode for SvfFilterNode<F> {
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Cutoff, self.frequency.as_atomic())
            .param(UnitParam::Q, self.q.as_atomic())
            .param(UnitParam::GainDb, self.gain_db.as_atomic())
            .build()
    }

    /// A clone with its cutoff, Q and gain cells detached (at their values
    /// now), so a write to either never reaches the other, and its
    /// integrators cleared.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.frequency.detach();
        fork.q.detach();
        fork.gain_db.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls (cutoff, Q and gain by
/// [`UnitParam`]) and a fork that starts from the values last set through
/// it ([`tutti_graph::param_parts`]).
impl<F: Real + 'static> IntoNode for SvfFilterNode<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

impl<F: Real> Clone for SvfFilterNode<F> {
    fn clone(&self) -> Self {
        Self {
            filter_type: self.filter_type,
            frequency: self.frequency.handle(),
            q: self.q.handle(),
            gain_db: self.gain_db.handle(),
            sample_rate: self.sample_rate,
            coeffs: self.coeffs,
            state: self.state.clone(),
            good: self.good,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_utils::{generate_sine, impulse, rms};
    use crate::test_support::{RATE, drive_block, prepared};
    use tutti_graph::contract::{assert_param_fork, drive};

    /// `node`, prepared at 44.1 kHz, over a mono `input` in one block.
    fn render(node: SvfFilterNode, input: &[f32]) -> Vec<f32> {
        let rate = SampleRate(44_100.0);
        let mut node = tutti_graph::contract::prepared(node, rate, input.len());
        drive(&mut node, rate, &[input], &[]).remove(0)
    }

    /// `node`, prepared at 44.1 kHz, over `inputs` (one slice per channel)
    /// in one block.
    fn render_wide(node: SvfFilterNode, inputs: &[&[f32]]) -> Vec<Vec<f32>> {
        let rate = SampleRate(44_100.0);
        let mut node = tutti_graph::contract::prepared(node, rate, inputs[0].len());
        drive(&mut node, rate, inputs, &[])
    }

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it. See
    /// `tutti_graph::contract::assert_param_fork` for what it pins.
    ///
    /// Mutation (run): drop the Q `detach` in `fork_fresh` → "a live write
    /// reached the fork" for `Q`. Mutation (run): leave `GainDb` out of
    /// `param_set` → the gain is unreachable by address, and the count
    /// assertion below fails.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let node = SvfFilterNode::<f64>::new(SvfType::Bell, 500.0, 0.707).with_gain_db(-3.0);
        assert_eq!(
            node.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Cutoff, UnitParam::Q, UnitParam::GainDb]
        );
        assert_param_fork(node);
    }

    #[test]
    fn test_svf_lowpass_attenuates_high_freq() {
        let make = || SvfFilterNode::<f64>::new(SvfType::LowPass, 500.0, 0.707);
        let out_low = render(make(), &generate_sine(100.0, 44100.0, 4096));
        let out_high = render(make(), &generate_sine(5000.0, 44100.0, 4096));
        let rms_low = rms(&out_low[512..]);
        let rms_high = rms(&out_high[512..]);
        assert!(
            rms_low > rms_high * 3.0,
            "LP should pass low freq ({rms_low}) much more than high ({rms_high})"
        );
    }

    #[test]
    fn test_svf_highpass_attenuates_low_freq() {
        let make = || SvfFilterNode::<f64>::new(SvfType::HighPass, 2000.0, 0.707);
        let out_low = render(make(), &generate_sine(100.0, 44100.0, 4096));
        let out_high = render(make(), &generate_sine(5000.0, 44100.0, 4096));
        let rms_low = rms(&out_low[512..]);
        let rms_high = rms(&out_high[512..]);
        assert!(
            rms_high > rms_low * 3.0,
            "HP should pass high freq ({rms_high}) much more than low ({rms_low})"
        );
    }

    #[test]
    fn test_svf_bandpass() {
        let make = || SvfFilterNode::<f64>::new(SvfType::BandPass, 1000.0, 5.0);
        let out_on = render(make(), &generate_sine(1000.0, 44100.0, 4096));
        let out_off = render(make(), &generate_sine(100.0, 44100.0, 4096));
        let rms_on = rms(&out_on[512..]);
        let rms_off = rms(&out_off[512..]);
        assert!(
            rms_on > rms_off * 2.0,
            "BP should favor center freq ({rms_on}) over off-freq ({rms_off})"
        );
    }

    #[test]
    fn test_svf_notch() {
        let make = || SvfFilterNode::<f64>::new(SvfType::Notch, 1000.0, 5.0);
        let out_on = render(make(), &generate_sine(1000.0, 44100.0, 4096));
        let out_off = render(make(), &generate_sine(5000.0, 44100.0, 4096));
        let rms_on = rms(&out_on[512..]);
        let rms_off = rms(&out_off[512..]);
        assert!(
            rms_off > rms_on * 2.0,
            "Notch should reject center ({rms_on}) vs off-freq ({rms_off})"
        );
    }

    #[test]
    fn test_svf_allpass_flat_magnitude() {
        // An allpass passes all frequencies at ~unity magnitude (it only shifts
        // phase). RMS out should track RMS in at both a low and a high tone,
        // unlike a notch which would attenuate near its centre.
        for hz in [200.0, 1000.0, 5000.0] {
            let sig = generate_sine(hz, 44100.0, 8192);
            let out = render(
                SvfFilterNode::<f64>::new(SvfType::Allpass, 1000.0, 0.707),
                &sig,
            );
            let (rin, rout) = (rms(&sig[1024..]), rms(&out[1024..]));
            let rel = (rout - rin).abs() / rin.max(1e-6);
            assert!(
                rel < 0.05,
                "allpass should preserve magnitude at {hz} Hz: in={rin}, out={rout} (rel {rel})"
            );
        }
    }

    #[test]
    fn test_svf_impulse_response_finite() {
        let ir = render(
            SvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707),
            &impulse(1024),
        );
        let first_half_energy: f32 = ir[..512].iter().map(|s| s * s).sum();
        let second_half_energy: f32 = ir[512..].iter().map(|s| s * s).sum();
        assert!(
            first_half_energy > second_half_energy,
            "IR should decay: first_half={first_half_energy}, second_half={second_half_energy}"
        );
    }

    /// `reset` clears the integrators: silence in is silence out.
    ///
    /// Mutation (run): an empty `Node::reset` → the ringing state leaks
    /// into the silent block → fails.
    #[test]
    fn test_svf_reset() {
        let mut filter = prepared(SvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707));
        drive_block(&mut filter, &[&generate_sine(100.0, 48_000.0, 100)]);
        Node::reset(&mut filter);
        let out = drive_block(&mut filter, &[&[0.0; 4]]);
        assert!(
            out[0].iter().all(|s| s.abs() < 1e-4),
            "After reset, output should be near zero: {:?}",
            out[0]
        );
    }

    /// The by-address path: a `UnitParam` set through the node's
    /// `ParamSet` lands in the same cell the bespoke setter writes, and an
    /// address the node does not own is refused.
    #[test]
    fn test_svf_set_via_unit_param() {
        let node = SvfFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            SvfType::LowPass,
            1000.0,
            0.707,
        );
        let params = node.param_set();
        assert!(params.set(UnitParam::Cutoff, 5000.0));
        assert!(params.set(UnitParam::Q, 2.5));
        assert_eq!(node.frequency.load(), Hz(5000.0));
        assert_eq!(node.q.load(), Q(2.5));
        assert!(!params.set(UnitParam::Wet, 0.5));
    }

    #[test]
    fn test_svf_parameter_modulation() {
        let noise: Vec<f32> = (0..1000)
            .map(|i| ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let out1 = render(
            SvfFilterNode::<f64>::new(SvfType::LowPass, 500.0, 0.707),
            &noise,
        );
        let high = SvfFilterNode::<f64>::new(SvfType::LowPass, 500.0, 0.707);
        high.set_frequency(5000.0);
        let out2 = render(high, &noise);
        let rms1 = rms(&out1[100..]);
        let rms2 = rms(&out2[100..]);
        assert!(
            rms2 > rms1,
            "Higher cutoff should pass more signal: low_cutoff={rms1}, high_cutoff={rms2}"
        );
    }

    #[test]
    fn test_svf_f32_state_matches_f64_for_mid_cutoff() {
        let input = generate_sine(500.0, 44100.0, 2048);
        let rate = SampleRate(44_100.0);
        let mut f64_node = tutti_graph::contract::prepared(
            SvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707),
            rate,
            2048,
        );
        let mut f32_node = tutti_graph::contract::prepared(
            SvfFilterNode::<f32>::new(SvfType::LowPass, 1000.0, 0.707),
            rate,
            2048,
        );
        let out_f64 = drive(&mut f64_node, rate, &[&input], &[]).remove(0);
        let out_f32 = drive(&mut f32_node, rate, &[&input], &[]).remove(0);
        let rms_f64 = rms(&out_f64[512..]);
        let rms_f32 = rms(&out_f32[512..]);
        let rel_diff = (rms_f64 - rms_f32).abs() / rms_f64.max(1e-6);
        assert!(
            rel_diff < 0.01,
            "f32 and f64 SVF should agree within 1% at mid cutoff: f64={rms_f64}, f32={rms_f32}"
        );
    }

    #[test]
    fn test_stereo_svf_matches_mono_per_channel() {
        // Identical signal on L+R must match what a mono SVF would produce.
        let signal = generate_sine(440.0, 44100.0, 2048);
        let mono_out = render(
            SvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707),
            &signal,
        );
        let out = render_wide(
            SvfFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                SvfType::LowPass,
                1000.0,
                0.707,
            ),
            &[&signal, &signal],
        );
        for i in 0..signal.len() {
            assert!(
                (mono_out[i] - out[0][i]).abs() < 1e-5,
                "L channel diverges from mono at sample {i}"
            );
            assert!(
                (mono_out[i] - out[1][i]).abs() < 1e-5,
                "R channel diverges from mono at sample {i}"
            );
        }
    }

    #[test]
    fn test_stereo_svf_per_channel_independence() {
        // Different signals on L vs R should not bleed across channels.
        let l_in = generate_sine(200.0, 44100.0, 2048);
        let r_in = vec![0.0f32; 2048];
        let out = render_wide(
            SvfFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                SvfType::LowPass,
                1000.0,
                0.707,
            ),
            &[&l_in, &r_in],
        );
        let l_energy: f32 = out[0].iter().map(|s| s * s).sum();
        let r_energy: f32 = out[1].iter().map(|s| s * s).sum();
        assert!(
            r_energy < 1e-10,
            "R should stay silent when only L has input; got energy {r_energy}"
        );
        assert!(l_energy > 1.0, "L should carry meaningful signal");
    }

    #[test]
    fn test_stereo_svf_reset_clears_both_channels() {
        let mut stereo = prepared(SvfFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            SvfType::LowPass,
            1000.0,
            0.707,
        ));
        let signal = generate_sine(100.0, 48_000.0, 200);
        drive_block(&mut stereo, &[&signal, &signal]);
        Node::reset(&mut stereo);
        let out = drive_block(&mut stereo, &[&[0.0], &[0.0]]);
        assert!(out[0][0].abs() < 1e-4 && out[1][0].abs() < 1e-4);
    }

    /// The shape is as wide as the filter was built, in and out, declares
    /// cutoff, Q and gain as its modulatable params, and reports
    /// `Tail::Unknown`. (Doc 014 §12.6 added the gain port: a route onto an
    /// SVF's gain was refused once the frame-rate driver went.)
    ///
    /// Mutation (run): swap `SVF_PARAMS`' order → the params assertion fails.
    /// Mutation (run): drop `.with_tail(Tail::Unknown)` → the shape's default
    /// `Tail::None` → the tail assertion fails. Mutation (run):
    /// `param_base(2)` answering the Q cell → fails.
    #[test]
    fn the_shape_is_the_width_and_declares_cutoff_q_and_gain() {
        let f = SvfFilterNode::<f64>::with_channels(6usize, SvfType::LowPass, 1000.0, 0.7);
        let shape = f.shape();
        assert_eq!((shape.audio_in.count(), shape.audio_out.count()), (6, 6));
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Cutoff, UnitParam::Q, UnitParam::GainDb][..]
        );
        assert_eq!(
            f.param_base(0),
            Some(1000.0),
            "cutoff's base is its control"
        );
        assert_eq!(f.param_base(1), Some(0.7), "Q's base is its control");
        assert_eq!(f.param_base(2), Some(0.0), "gain's base is its control");
        assert_eq!(shape.tail, Tail::Unknown, "a resonant filter rings on");
    }

    #[test]
    fn wide_channel_matches_mono_and_is_independent() {
        // Each of 6 channels must filter exactly like a mono SVF with the same
        // coeffs, and carry only its own input (no cross-channel bleed).
        let sig = generate_sine(300.0, 44100.0, 2048);
        let silent = vec![0.0f32; sig.len()];
        let mut frames: Vec<&[f32]> = vec![&silent; 6];
        frames[4] = &sig;
        let out = render_wide(
            SvfFilterNode::<f64>::with_channels(6usize, SvfType::LowPass, 1000.0, 0.707),
            &frames,
        );
        let mono_out = render(
            SvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707),
            &sig,
        );
        for i in 0..sig.len() {
            assert!(
                (mono_out[i] - out[4][i]).abs() < 1e-5,
                "ch4 diverges from mono at {i}"
            );
        }
        for c in [0usize, 1, 2, 3, 5] {
            let e: f32 = out[c].iter().map(|s| s * s).sum();
            assert!(e < 1e-10, "ch{c} should stay silent; energy {e}");
        }
    }

    // ── Modulated params (the graph's param ports) ──────────────────────────

    #[test]
    fn stereo_svf_fed_cutoff_modulates_response() {
        // Same noise through a low-pass: a cutoff fed high should pass more
        // energy than the same node fed low. Proves the param port actually
        // drives the coefficients.
        let noise: Vec<f32> = (0..2048)
            .map(|i| ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let run = |cutoff: f32| -> f32 {
            let mut f = prepared(SvfFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                SvfType::LowPass,
                200.0,
                0.707,
            ));
            let held = vec![cutoff; noise.len()];
            let out = drive(&mut f, RATE, &[&noise, &noise], &[Some(&held), None]);
            rms(&out[0][256..])
        };
        let low = run(200.0);
        let high = run(8000.0);
        assert!(
            high > low * 1.5,
            "higher fed cutoff should pass more: low={low}, high={high}"
        );
    }

    #[test]
    fn stereo_svf_unmodulated_process_matches_modulated_held_constant() {
        // A node whose fed cutoff is held at the same value as an unmodulated
        // node's cell must produce the same output — the modulated path is a
        // faithful superset.
        let signal = generate_sine(440.0, 48_000.0, 1024);
        let make = || {
            prepared(SvfFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                SvfType::LowPass,
                1000.0,
                0.707,
            ))
        };
        let plain = drive_block(&mut make(), &[&signal, &signal]);
        let held = vec![1000.0f32; signal.len()];
        let modn = drive(&mut make(), RATE, &[&signal, &signal], &[Some(&held), None]);
        for i in 0..signal.len() {
            assert!(
                (plain[0][i] - modn[0][i]).abs() < 1e-5,
                "modulated-held output diverges from plain at sample {i}: {} vs {}",
                plain[0][i],
                modn[0][i]
            );
        }
    }

    // ── Per-block reads ──────────────────────────────────────────────────────

    /// A cutoff change made between blocks is read by the next block and
    /// ramped across it, and that block ends solved exactly at the new cutoff.
    ///
    /// (A write *during* a block cannot be staged in a single-threaded test —
    /// the control thread writes between callbacks — so "between blocks" is the
    /// observable form of "not before the next block".)
    ///
    /// Mutation: solving straight at the target and rendering the block held
    /// (a jump) fails `assert_ramps_in`; dropping the `self.coeffs = prev`
    /// that stores the block's final solve fails the coefficient check.
    #[test]
    fn a_cutoff_change_is_read_next_block_and_ramped_across_it() {
        use crate::test_support::{change_between_node_blocks, noise};
        let x = noise(3, 128);
        let run = change_between_node_blocks(
            || {
                SvfFilterNode::<f64>::with_channels(
                    ChannelLayout::STEREO,
                    SvfType::LowPass,
                    400.0,
                    0.707,
                )
            },
            |n| n.set_frequency(6_000.0),
            &[&x[..64], &x[..64]],
            &[&x[64..], &x[64..]],
        );
        run.assert_ramps_in("svf cutoff");
        assert!(
            run.ramped.coeffs == run.jumped.coeffs,
            "the block must end solved at the new cutoff"
        );
    }

    /// A NaN in a raw cell never reaches the filter state — not even in the
    /// block where another control moves and forces a re-solve, which is the
    /// case a threshold comparison alone lets through. The filter keeps
    /// running at the last good cutoff, and picks up the next finite write.
    ///
    /// Mutation: reading the cutoff cell raw in `read_controls` (no
    /// `LastGood`) fails the second assertion — the NaN reaches the solve and
    /// the output goes NaN for good.
    #[test]
    fn a_nan_in_a_raw_cell_never_reaches_the_state() {
        use crate::test_support::noise;
        let x = noise(31, 64);
        let mut f = prepared(SvfFilterNode::<f64>::new(SvfType::LowPass, 800.0, 0.7));
        drive_block(&mut f, &[&x]);
        f.frequency()
            .store(f32::NAN, std::sync::atomic::Ordering::Release);
        let held = drive_block(&mut f, &[&x]);
        assert!(held[0].iter().all(|s| s.is_finite()), "a NaN alone");
        f.set_q(3.0);
        let moved = drive_block(&mut f, &[&x]);
        assert!(
            moved[0].iter().all(|s| s.is_finite()),
            "a NaN with another control moving in the same block"
        );
        assert_eq!(
            f.coeffs.last_freq,
            Hz(800.0),
            "held at the last good cutoff"
        );
        f.set_frequency(2_000.0);
        drive_block(&mut f, &[&x]);
        assert_eq!(
            f.coeffs.last_freq,
            Hz(2_000.0),
            "and back on a finite write"
        );
    }
}
